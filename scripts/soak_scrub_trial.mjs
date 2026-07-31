#!/usr/bin/env node
/**
 * One first-scrub resume trial (Chrome / hls.js). Driven by soak_scrub.sh.
 *
 * Env:
 *   BASE          nightjar origin (default http://127.0.0.1:PORT)
 *   ITEM          item id
 *   CELL          a|b|c|d  (nosubs / soft / burn_ass / burn_pgs)
 *   AXIS          behind_head | far_ahead
 *   SOFT_ID       soft WebVTT track id (cell b)
 *   ASS_ID / PGS_ID  burn-in track ids (cells c/d)
 *   OUT           JSON result path
 *   LOG_MARK      wall-clock ISO written by the shell around this trial
 *
 * Records: client requests, hls.js FRAG_LOADED / BUFFER_APPENDED / ERROR,
 * whether playback resumed after the scrub. Server "served" lines are
 * correlated later by soak_scrub.sh from the access / hls_client_req log.
 *
 * Segment probes abort after 2.5s so we never sit in the server's 30s
 * SEGMENT_WAIT hold (that was hanging CDP and leaking Chrome between trials).
 */
import { spawn } from "node:child_process";
import { writeFileSync, rmSync, readFileSync, existsSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..");

const BASE = (process.env.BASE || "http://127.0.0.1:18098").replace(/\/$/, "");
const ITEM = process.env.ITEM || "1";
const CELL = process.env.CELL || "a";
const AXIS = process.env.AXIS || "far_ahead";
const SOFT_ID = process.env.SOFT_ID || "";
const ASS_ID = process.env.ASS_ID || "";
const PGS_ID = process.env.PGS_ID || "";
const OUT = process.env.OUT || "/tmp/nj-soak-trial.json";
const LOG_MARK = process.env.LOG_MARK || new Date().toISOString();
const SEGMENT_MS = 2000;
/** ALIGN_BEHIND_SEGMENTS in hls.rs — seek must clear this band. */
const ALIGN_BEHIND_MS = 16 * SEGMENT_MS;
const RESUME_WAIT_MS = Number(process.env.RESUME_WAIT_MS || "25000");
const ADVANCE_S = Number(process.env.ADVANCE_S || "1.5");
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const cdpPort = Number(process.env.CDP_PORT || "19551");

function wallIso() {
  return new Date().toISOString();
}

function timeKeyedName(startMs) {
  return `seg_${String(startMs).padStart(11, "0")}.m4s`;
}

/** Wait for a time-keyed segment URI (from playlist / known land ms). */
async function waitListedSeg(sessionId, startMs, timeoutMs = 45000) {
  // One long-poll ≈ SEGMENT_WAIT (30s). Abort spam never sees a 200 that
  // arrives at second 3 of a hold, and burned the 100s trial wall before
  // the player ran.
  const url = `${BASE}/api/v0/sessions/${sessionId}/${timeKeyedName(startMs)}`;
  const t0 = Date.now();
  let lastStatus = 0;
  let attempt = 0;
  for (;;) {
    const left = timeoutMs - (Date.now() - t0);
    if (left <= 0) {
      return { ok: false, status: lastStatus || 0, ms: Date.now() - t0, timeout: true };
    }
    attempt += 1;
    const slice = Math.min(35000, left);
    process.stderr.write(
      `  waitSeg ${timeKeyedName(startMs)} attempt=${attempt} slice=${slice}ms\n`,
    );
    try {
      const res = await fetch(url, { signal: AbortSignal.timeout(slice) });
      lastStatus = res.status;
      if (res.ok) {
        await res.arrayBuffer();
        process.stderr.write(
          `  waitSeg ${timeKeyedName(startMs)} ok in ${Date.now() - t0}ms\n`,
        );
        return { ok: true, status: res.status, ms: Date.now() - t0 };
      }
      if (res.status === 404) {
        return { ok: false, status: 404, ms: Date.now() - t0 };
      }
      // 503 / other: brief pause then retry
      await sleep(200);
    } catch {
      await sleep(200);
    }
  }
}

async function firstListedStartMs(masterUrl) {
  const indexUrl = masterUrl.replace(/\/master\.m3u8(?:\?.*)?$/i, "/index.m3u8");
  const res = await fetch(indexUrl, { signal: AbortSignal.timeout(5000) }).catch(
    () => null,
  );
  if (!res || !res.ok) return null;
  const text = await res.text();
  for (const line of text.split("\n")) {
    const name = line.trim().replace(/^\.\.\//, "");
    const m = /^seg_(\d{11})\.m4s$/.exec(name);
    if (m) return Number(m[1]);
  }
  return null;
}

function cellSubtitleTrackId() {
  if (CELL === "c") return ASS_ID || null;
  if (CELL === "d") return PGS_ID || null;
  return null;
}

async function main() {
  const tWall0 = wallIso();
  const marks = [];
  const mark = (phase, detail) => {
    marks.push({ phase, wall: wallIso(), detail: detail || null });
  };

  let chromeProc = null;
  let server = null;
  let sessionId = null;
  const userDataDir = `/tmp/nj-soak-chrome-${process.pid}-${cdpPort}`;

  const cleanup = async () => {
    if (sessionId) {
      await fetch(`${BASE}/api/v0/sessions/${sessionId}`, { method: "DELETE" }).catch(() => {});
      sessionId = null;
    }
    if (chromeProc && chromeProc.exitCode == null) {
      try {
        chromeProc.kill("SIGKILL");
      } catch {
        /* ignore */
      }
    }
    if (server) {
      try {
        server.close();
      } catch {
        /* ignore */
      }
    }
    try {
      rmSync(userDataDir, { recursive: true, force: true });
    } catch {
      /* ignore */
    }
  };

  try {
    process.stderr.write(`  phase=chrome_cdp port=${cdpPort}\n`);
    chromeProc = spawn(
      chrome,
      [
        `--remote-debugging-port=${cdpPort}`,
        "--remote-allow-origins=*",
        "--headless=new",
        "--disable-gpu",
        "--disable-web-security",
        "--autoplay-policy=no-user-gesture-required",
        "--no-first-run",
        "--no-default-browser-check",
        `--user-data-dir=${userDataDir}`,
        "about:blank",
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    let chromeErr = "";
    chromeProc.stderr?.on("data", (d) => {
      chromeErr += String(d);
      if (chromeErr.length > 4000) chromeErr = chromeErr.slice(-4000);
    });

    let version;
    for (let i = 0; i < 150; i++) {
      if (chromeProc.exitCode != null) break;
      try {
        version = await fetch(`http://127.0.0.1:${cdpPort}/json/version`).then((r) => r.json());
        break;
      } catch {
        await sleep(200);
      }
    }
    if (!version) {
      throw new Error(
        `Chrome CDP not up on :${cdpPort} (exit=${chromeProc.exitCode}). stderr: ${chromeErr.slice(-800) || "(empty)"}`,
      );
    }

    const info = await fetch(`${BASE}/api/v0/items/${ITEM}/playback-info`).then((r) => {
      if (!r.ok) throw new Error(`playback-info ${r.status}`);
      return r.json();
    });
    const durationMs = Number(info.durationMs || 0);
    if (durationMs < 90_000) {
      throw new Error(`fixture too short (${durationMs}ms); need >=90s for axes`);
    }

    let attachStartMs;
    let scrubMs;
    if (AXIS === "behind_head") {
      attachStartMs = Math.min(90_000, Math.floor(durationMs * 0.45));
      scrubMs = Math.max(0, attachStartMs - (ALIGN_BEHIND_MS + 8_000));
    } else if (AXIS === "far_ahead") {
      attachStartMs = 0;
      scrubMs = Math.min(120_000, Math.floor(durationMs * 0.7));
      if (scrubMs < ALIGN_BEHIND_MS + 16_000) {
        scrubMs = ALIGN_BEHIND_MS + 16_000;
      }
    } else {
      throw new Error(`unknown AXIS ${AXIS}`);
    }

    const burnId = cellSubtitleTrackId();
    let postUrl = `${BASE}/api/v0/items/${ITEM}/sessions?startMs=${attachStartMs}`;
    if (burnId) postUrl += `&subtitleTrackId=${encodeURIComponent(burnId)}`;

    mark("session_post", { url: postUrl });
    process.stderr.write(`  phase=session_post attachMs=${attachStartMs} scrubMs=${scrubMs}\n`);
    const started = await fetch(postUrl, { method: "POST" }).then(async (r) => {
      const body = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(`POST session ${r.status}: ${JSON.stringify(body)}`);
      return body;
    });
    sessionId = started.sessionId;
    const masterUrl = `${BASE}${started.playlistUrl}`;
    mark("session_ok", { sessionId, playlistUrl: started.playlistUrl });

    for (let i = 0; i < 600; i++) {
      const res = await fetch(masterUrl, { signal: AbortSignal.timeout(3000) }).catch(() => null);
      if (res && res.ok) {
        await res.text();
        mark("master_ready", { status: res.status });
        break;
      }
      await sleep(100);
    }

    const landIdxAttach = Math.floor(attachStartMs / SEGMENT_MS);
    const encodeStartMs = Math.max(
      0,
      Math.floor(attachStartMs / SEGMENT_MS) * SEGMENT_MS - 8 * SEGMENT_MS,
    );
    process.stderr.write(`  phase=first_seg startMs=${encodeStartMs}\n`);
    let firstStart = await firstListedStartMs(masterUrl);
    if (firstStart == null) firstStart = encodeStartMs;
    const first = await waitListedSeg(sessionId, firstStart, 45000);
    mark("first_seg", { startMs: firstStart, ...first });
    if (!first.ok) {
      writeFileSync(
        OUT,
        JSON.stringify(
          {
            cell: CELL,
            axis: AXIS,
            resumed: false,
            error: "first_seg_timeout",
            sessionId,
            marks,
            logMark: LOG_MARK,
            tWall0,
          },
          null,
          2,
        ),
      );
      await cleanup();
      process.exit(2);
    }

    if (AXIS === "behind_head") {
      // One segment past land is enough to call the head "ahead"; keep this
      // short so the player still fits under the trial wall.
      const aheadMs = landIdxAttach * SEGMENT_MS + SEGMENT_MS;
      process.stderr.write(`  phase=encoder_head_seg startMs=${aheadMs}\n`);
      const cooked = await waitListedSeg(sessionId, aheadMs, 40000);
      mark("encoder_head_seg", { startMs: aheadMs, ...cooked });
    }

    process.stderr.write(`  phase=client_run\n`);

    const enableSoft = CELL === "b";
    const scrubS = scrubMs / 1000;
    const attachS = attachStartMs / 1000;
    const runDeadlineMs = 20000 + RESUME_WAIT_MS * 2 + 5000;

    // Serve local hls.js — CDN hangs have wedged headless before.
    const hlsPath = join(REPO_ROOT, "web/node_modules/hls.js/dist/hls.min.js");
    const hlsJs = existsSync(hlsPath) ? readFileSync(hlsPath) : null;
    if (!hlsJs) {
      process.stderr.write(
        "  WARN: web/node_modules/hls.js missing; page will use CDN\n",
      );
    }

    const html = `<!doctype html><html><body>
<video id="v" controls playsinline muted></video>
${hlsJs ? '<script src="/hls.min.js"><\/script>' : '<script src="https://cdn.jsdelivr.net/npm/hls.js@1.5.17/dist/hls.min.js"><\/script>'}
<script>
const t0 = performance.now();
const wall0 = Date.now();
const events = [];
const reqs = [];
const push = (kind, detail) => {
  const ms = Math.round(performance.now() - t0);
  const wall = new Date(wall0 + ms).toISOString();
  events.push({ kind, ms, wall, detail: detail || null });
  console.info('[nj-soak]', wall, kind, detail ? JSON.stringify(detail) : '');
};
const race = (p, ms, label) => Promise.race([
  p,
  new Promise((_, rej) => setTimeout(() => rej(new Error(label || 'timeout')), ms)),
]);
const playBrief = () => race(v.play(), 2000, 'play_timeout').catch(() => {});
const v = document.getElementById('v');
const origFetch = window.fetch.bind(window);
window.fetch = async (input, init) => {
  const url = typeof input === 'string' ? input : input.url;
  const t = Math.round(performance.now() - t0);
  const wall = new Date(wall0 + t).toISOString();
  // Cap session GETs so a SEGMENT_WAIT hold cannot wedge the page forever.
  const opts = Object.assign({}, init || {});
  if (url.includes('/sessions/') && !opts.signal) {
    opts.signal = AbortSignal.timeout(8000);
  }
  let res;
  try {
    res = await origFetch(input, opts);
  } catch (e) {
    if (url.includes('/sessions/')) {
      reqs.push({ ms: t, wall, status: 0, resource: String(url).split('/').pop(), error: String(e) });
    }
    throw e;
  }
  if (url.includes('/sessions/')) {
    const u = new URL(url, location.origin);
    const resource = u.pathname.split('/').pop();
    const startMs = u.searchParams.get('startMs');
    const row = {
      ms: t,
      wall,
      status: res.status,
      resource,
      startMs: startMs != null ? Number(startMs) : null,
      url: u.pathname + u.search,
    };
    reqs.push(row);
    push('client_req', row);
  }
  return res;
};
async function probe(url, ms) {
  try {
    const res = await origFetch(url, { signal: AbortSignal.timeout(ms) });
    if (res.ok) { try { await res.arrayBuffer(); } catch (_) {} }
    return res.status;
  } catch (_) {
    return 0;
  }
}
if (typeof Hls === 'undefined') {
  push('fatal', { error: 'Hls_undefined' });
  window.__NJ = { run: async () => ({ resumed: false, error: 'Hls_undefined', events, reqs }) };
} else {
const hls = new Hls({
  startPosition: ${attachS},
  enableWorker: false,
  maxBufferHole: 1.5,
  fragLoadingTimeOut: 8000,
  manifestLoadingTimeOut: 8000,
  levelLoadingTimeOut: 8000,
});
window.__njHls = hls;
hls.on(Hls.Events.FRAG_LOADED, (_, data) => {
  const f = data.frag;
  push('FRAG_LOADED', {
    type: f.type,
    sn: f.sn,
    start: f.start,
    duration: f.duration,
    url: f.url ? f.url.split('/').slice(-2).join('/') : null,
  });
});
hls.on(Hls.Events.BUFFER_APPENDED, (_, data) => {
  push('BUFFER_APPENDED', { type: data.type });
});
hls.on(Hls.Events.ERROR, (_, data) => {
  push('ERROR', {
    type: data.type,
    details: data.details,
    fatal: data.fatal,
    response: data.response ? { code: data.response.code } : null,
  });
});
hls.loadSource(${JSON.stringify(masterUrl)});
hls.attachMedia(v);
v.addEventListener('playing', () => push('playing', { t: v.currentTime }), { once: true });
playBrief();

async function selectSoft() {
  if (!${enableSoft ? "true" : "false"}) return;
  for (let i = 0; i < 50; i++) {
    if (hls.subtitleTracks && hls.subtitleTracks.length > 0) {
      hls.subtitleDisplay = true;
      hls.subtitleTrack = 0;
      push('soft_selected', { n: hls.subtitleTracks.length });
      return;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  push('soft_missing', null);
}

async function scrubTo(targetS) {
  push('scrub_intent', { targetS });
  const startMs = Math.max(0, Math.floor(targetS * 1000));
  const seekUrl = ${JSON.stringify(BASE)} + '/api/v0/sessions/' + ${JSON.stringify(sessionId)} + '/seek?startMs=' + startMs;
  const t = Math.round(performance.now() - t0);
  const wall = new Date(wall0 + t).toISOString();
  let status = 0;
  let newPlaylist = null;
  try {
    const res = await origFetch(seekUrl, { method: 'POST', signal: AbortSignal.timeout(5000) });
    status = res.status;
    if (res.ok) {
      const body = await res.json();
      newPlaylist = body.playlistUrl ? (${JSON.stringify(BASE)} + body.playlistUrl) : null;
    }
  } catch (_) {
    status = 0;
  }
  reqs.push({ ms: t, wall, status, resource: 'seek', startMs });
  push('client_req_seek', { status, startMs, playlistUrl: newPlaylist });
  if (newPlaylist && window.__njHls) {
    window.__njHls.loadSource(newPlaylist);
    window.__njHls.startLoad(targetS);
  }
  v.currentTime = targetS;
  await playBrief();
}

function snap() {
  const ranges = [];
  try {
    for (let i = 0; i < v.buffered.length; i++) {
      ranges.push([v.buffered.start(i), v.buffered.end(i)]);
    }
  } catch (_) {}
  return {
    currentTime: v.currentTime,
    paused: v.paused,
    seeking: v.seeking,
    readyState: v.readyState,
    buffered: ranges,
  };
}

async function runInner() {
  push('run_start', { attachS: ${attachS}, scrubS: ${scrubS} });
  await selectSoft();
  const attachDeadline = performance.now() + 15000;
  while (performance.now() < attachDeadline) {
    if (v.readyState >= 2 && Math.abs(v.currentTime - ${attachS}) < 8) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  push('pre_scrub', snap());
  await scrubTo(${scrubS});
  const landMs = Math.floor(${scrubMs} / ${SEGMENT_MS}) * ${SEGMENT_MS};
  const landUrl = ${JSON.stringify(BASE)} + '/api/v0/sessions/' + ${JSON.stringify(sessionId)} + '/' +
    ('seg_' + String(landMs).padStart(11, '0') + '.m4s');
  const landDeadline = performance.now() + ${RESUME_WAIT_MS};
  let landStatus = null;
  while (performance.now() < landDeadline) {
    landStatus = await probe(landUrl, 2000);
    if (landStatus === 200) {
      push('land_seg_ok', { startMs: landMs, status: landStatus });
      break;
    }
    await new Promise((r) => setTimeout(r, 150));
  }
  if (landStatus !== 200) push('land_seg_miss', { startMs: landMs, status: landStatus });

  const tLand = v.currentTime;
  push('post_land', snap());
  // Resume = playhead advances past the seek target (not merely FRAG/BUFFER
  // events — those fire for a stuck keyframe too). readyState must be able
  // to play forward (HAVE_FUTURE_DATA+).
  const resumeDeadline = performance.now() + ${RESUME_WAIT_MS};
  let resumed = false;
  let last = snap();
  while (performance.now() < resumeDeadline) {
    last = snap();
    const pastTarget = last.currentTime >= ${scrubS} + ${ADVANCE_S};
    const moved = last.currentTime >= tLand + ${ADVANCE_S};
    if (
      !last.paused &&
      last.readyState >= 3 &&
      pastTarget &&
      moved
    ) {
      resumed = true;
      break;
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  push('final', { ...last, resumed, tLand, scrubS: ${scrubS}, advanceS: ${ADVANCE_S} });
  return { resumed, final: last, events, reqs };
}

window.__NJ = {
  ready: true,
  events,
  reqs,
  snap,
  async run() {
    // Always settle — CDP awaitPromise must not hang the soak.
    try {
      return await race(runInner(), ${runDeadlineMs}, 'run_deadline');
    } catch (e) {
      push('run_error', { error: String(e), snap: snap() });
      return { resumed: false, error: String(e), final: snap(), events, reqs };
    }
  },
};
}
<\/script></body></html>`;

    server = createServer((req, res) => {
      if (hlsJs && req.url === "/hls.min.js") {
        res.writeHead(200, { "content-type": "application/javascript" });
        res.end(hlsJs);
        return;
      }
      res.writeHead(200, { "content-type": "text/html" });
      res.end(html);
    });
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    const pageUrl = `http://127.0.0.1:${server.address().port}/`;

    const ws = new WebSocket(version.webSocketDebuggerUrl);
    await Promise.race([
      new Promise((res, rej) => {
        ws.addEventListener("open", res);
        ws.addEventListener("error", rej);
      }),
      sleep(10000).then(() => {
        throw new Error("CDP websocket open timeout");
      }),
    ]);
    let id = 0;
    const onceMessage = (pred, timeoutMs = 30000) =>
      new Promise((resolve, reject) => {
        const t = setTimeout(() => reject(new Error("CDP timeout")), timeoutMs);
        ws.addEventListener("message", function onMsg(ev) {
          const msg = JSON.parse(ev.data);
          if (pred(msg)) {
            clearTimeout(t);
            ws.removeEventListener("message", onMsg);
            resolve(msg);
          }
        });
      });
    const send = (method, params = {}, sid, timeoutMs) => {
      const msg = { id: ++id, method, params };
      if (sid) msg.sessionId = sid;
      ws.send(JSON.stringify(msg));
      return onceMessage((m) => m.id === msg.id, timeoutMs).then((m) => {
        if (m.error) throw new Error(JSON.stringify(m.error));
        return m.result;
      });
    };

    const consoleLines = [];
    ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.method === "Runtime.consoleAPICalled") {
        const text = (msg.params.args || [])
          .map((a) => a.value ?? a.description ?? "")
          .join(" ");
        if (String(text).includes("[nj-soak]")) {
          consoleLines.push(String(text));
          process.stderr.write(`  ${String(text)}\n`);
        }
      }
    });

    const { targetId } = await send("Target.createTarget", { url: "about:blank" });
    const { sessionId: cdpSession } = await send("Target.attachToTarget", {
      targetId,
      flatten: true,
    });
    await send("Page.enable", {}, cdpSession);
    await send("Runtime.enable", {}, cdpSession);

    const loadFired = onceMessage(
      (m) => m.method === "Page.loadEventFired",
      20000,
    ).catch(() => null);
    await send("Page.navigate", { url: pageUrl }, cdpSession);
    await loadFired;

    // Wait until the inline script defined __NJ (hls.js parsed).
    let ready = false;
    for (let i = 0; i < 50; i++) {
      const probeNj = await send(
        "Runtime.evaluate",
        { expression: "!!(window.__NJ && window.__NJ.run)", returnByValue: true },
        cdpSession,
        5000,
      );
      if (probeNj?.result?.value) {
        ready = true;
        break;
      }
      await sleep(200);
    }
    if (!ready) {
      throw new Error("window.__NJ never became ready (hls.js/page script failed)");
    }
    process.stderr.write(`  phase=nj_ready\n`);

    mark("client_run_start", null);
    const runBudgetMs = runDeadlineMs + 15000;
    const run = await send(
      "Runtime.evaluate",
      {
        expression: "window.__NJ.run()",
        awaitPromise: true,
        returnByValue: true,
      },
      cdpSession,
      runBudgetMs,
    );
    const client = run?.result?.value || {};
    mark("client_run_done", { resumed: client.resumed, error: client.error || null });
    process.stderr.write(
      `  phase=client_done resumed=${Boolean(client.resumed)} err=${client.error || "-"}\n`,
    );

    const result = {
      cell: CELL,
      axis: AXIS,
      sessionId,
      attachStartMs,
      scrubMs,
      burnTrackId: burnId,
      softTrackId: enableSoft ? SOFT_ID : null,
      resumed: Boolean(client.resumed),
      clientFinal: client.final || null,
      clientRequested: client.reqs || [],
      hlsEvents: (client.events || []).filter((e) =>
        ["FRAG_LOADED", "BUFFER_APPENDED", "ERROR", "scrub_intent", "land_seg_ok", "final"].includes(
          e.kind,
        ),
      ),
      allEvents: client.events || [],
      consoleLines,
      serverMarks: marks,
      logMark: LOG_MARK,
      tWall0,
      tWall1: wallIso(),
    };

    writeFileSync(OUT, JSON.stringify(result, null, 2));
    console.log(
      JSON.stringify({
        cell: CELL,
        axis: AXIS,
        resumed: result.resumed,
        sessionId,
        scrubMs,
        out: OUT,
      }),
    );

    await cleanup();
    process.exit(result.resumed ? 0 : 3);
  } catch (e) {
    await cleanup();
    throw e;
  }
}

main().catch((e) => {
  console.error(e);
  writeFileSync(
    OUT,
    JSON.stringify({ cell: CELL, axis: AXIS, resumed: false, error: String(e) }, null, 2),
  );
  process.exit(1);
});
