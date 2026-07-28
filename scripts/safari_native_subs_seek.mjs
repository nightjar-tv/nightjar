#!/usr/bin/env node
/**
 * Reproduce / verify WebKit native HLS subtitle reassert after seek.
 *
 * Uses Playwright bundled WebKit (no Safari "Allow remote automation").
 * Same-origin proxies the Nightjar session so media + TEXT load like a page.
 *
 *   BASE=http://127.0.0.1:8098 ITEM=33 START_MS=120000 SEEK_TO_MS=300000 \
 *     REASSERT=none|teardown node scripts/safari_native_subs_seek.mjs
 */
import { createServer } from "node:http";
import { setTimeout as sleep } from "node:timers/promises";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdirSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const VENDOR = join(__dirname, ".tools");
const BASE = (process.env.BASE || "http://127.0.0.1:8098").replace(/\/$/, "");
const ITEM = process.env.ITEM || "33";
const START_MS = Number(process.env.START_MS || "120000");
const SEEK_TO_MS = Number(process.env.SEEK_TO_MS || "300000");
const REASSERT = process.env.REASSERT || "none";
const POLL_MS = Number(process.env.POLL_MS || "250");
const AFTER_SEEK_S = Number(process.env.AFTER_SEEK_S || "10");
const LINEAR_WAIT_S = Number(process.env.LINEAR_WAIT_S || "45");
const HARNESS_PORT = Number(process.env.HARNESS_PORT || "8765");

function ensurePlaywright() {
  mkdirSync(VENDOR, { recursive: true });
  const pwPkg = join(VENDOR, "node_modules", "playwright");
  if (!existsSync(pwPkg)) {
    console.log(`installing playwright into ${VENDOR} (gitignored)...`);
    const r = spawnSync(
      "npm",
      ["install", "--no-package-lock", "--prefix", VENDOR, "playwright@1.49.1"],
      { stdio: "inherit" }
    );
    if (r.status !== 0) throw new Error("npm install playwright failed");
  }
  spawnSync(
    join(VENDOR, "node_modules", ".bin", "playwright"),
    ["install", "webkit"],
    { stdio: "inherit", env: process.env }
  );
  const require = createRequire(join(VENDOR, "package.json"));
  return require("playwright");
}

async function httpJson(method, url) {
  const res = await fetch(url, { method });
  const text = await res.text();
  let body = {};
  try {
    body = text ? JSON.parse(text) : {};
  } catch {
    body = { raw: text.slice(0, 200) };
  }
  return { status: res.status, body };
}

async function httpText(url) {
  const res = await fetch(url);
  const buf = Buffer.from(await res.arrayBuffer());
  return {
    status: res.status,
    contentType: res.headers.get("content-type") || "",
    body: buf,
    headers: res.headers
  };
}

async function waitOk(url, timeoutS = 120) {
  const deadline = Date.now() + timeoutS * 1000;
  while (Date.now() < deadline) {
    try {
      const { status, body } = await httpText(url);
      if (status === 200 && body.length > 0) return true;
    } catch {
      /* retry */
    }
    await sleep(250);
  }
  return false;
}

function findSubUri(masterText) {
  for (const line of masterText.split("\n")) {
    if (line.includes("TYPE=SUBTITLES") && line.includes("URI=")) {
      const m = line.match(/URI="([^"]+)"/);
      if (m) return m[1];
    }
  }
  return null;
}

function harnessHtml(proxyMaster, startS, seekS) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>nj safari subs seek</title></head>
<body>
<video id="v" controls playsinline muted></video>
<script>
window.__nj = { phase: 'boot', samples: [], error: null };
const PLAYLIST = ${JSON.stringify(proxyMaster)};
const START_S = ${startS};
const SEEK_S = ${seekS};
const REASSERT = ${JSON.stringify(REASSERT)};
const POLL_MS = ${POLL_MS};
const LINEAR_WAIT_MS = ${Math.floor(LINEAR_WAIT_S * 1000)};
const AFTER_SEEK_MS = ${Math.floor(AFTER_SEEK_S * 1000)};

function snap(label) {
  const v = document.getElementById('v');
  const list = v.textTracks;
  const tracks = [];
  for (let i = 0; i < list.length; i++) {
    const t = list[i];
    const cues = t.cues;
    const active = t.activeCues;
    let cover = false;
    if (active) {
      for (let j = 0; j < active.length; j++) {
        const c = active[j];
        if (c.startTime <= v.currentTime && v.currentTime < c.endTime) cover = true;
      }
    }
    tracks.push({
      i,
      label: t.label || t.language || t.kind,
      mode: t.mode,
      cues: cues ? cues.length : null,
      active: active ? active.length : null,
      cover
    });
  }
  window.__nj.samples.push({
    t: Math.round(performance.now()),
    label,
    currentTime: +v.currentTime.toFixed(3),
    seeking: v.seeking,
    readyState: v.readyState,
    networkState: v.networkState,
    tracks
  });
}

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

async function waitCues(timeoutMs) {
  const v = document.getElementById('v');
  const t0 = performance.now();
  while (performance.now() - t0 < timeoutMs) {
    for (let i = 0; i < v.textTracks.length; i++) {
      const c = v.textTracks[i].cues;
      if (c && c.length > 0) return true;
    }
    await sleep(200);
  }
  return false;
}

function coveringActive(v) {
  for (let i = 0; i < v.textTracks.length; i++) {
    const active = v.textTracks[i].activeCues;
    if (!active) continue;
    for (let j = 0; j < active.length; j++) {
      const c = active[j];
      if (c.startTime <= v.currentTime && v.currentTime < c.endTime) return true;
    }
  }
  return false;
}

function waitCuesNull(track, timeoutMs) {
  return new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (!track.cues) return resolve(true);
      if (performance.now() - t0 >= timeoutMs) return resolve(false);
      setTimeout(tick, 50);
    };
    tick();
  });
}

function waitCoveringCue(track, video, timeoutMs) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (ok) => {
      if (done) return;
      done = true;
      track.removeEventListener('cuechange', onCue);
      resolve(ok);
    };
    const onCue = () => { if (coveringActive(video)) finish(true); };
    track.addEventListener('cuechange', onCue);
    if (coveringActive(video)) finish(true);
    setTimeout(() => finish(coveringActive(video)), timeoutMs);
  });
}

async function reassertTeardown(v) {
  const wanted = 0;
  snap('reassert:disable');
  for (let i = 0; i < v.textTracks.length; i++) v.textTracks[i].mode = 'disabled';
  const track = v.textTracks[wanted];
  const dropped = await waitCuesNull(track, 5000);
  snap(dropped ? 'reassert:cues-dropped' : 'reassert:cues-drop-timeout');
  track.mode = 'showing';
  snap('reassert:showing');
  const covered = await waitCoveringCue(track, v, 10000);
  snap(covered ? 'reassert:cover-ok' : 'reassert:cover-timeout');
  return covered;
}

async function run() {
  const v = document.getElementById('v');
  window.__nj.phase = 'attach';
  v.src = START_S > 0 ? (PLAYLIST + '#t=' + START_S) : PLAYLIST;
  await new Promise((resolve, reject) => {
    v.addEventListener('loadedmetadata', () => {
      if (START_S > 0 && Math.abs(v.currentTime - START_S) > 1) v.currentTime = START_S;
      snap('loadedmetadata');
      resolve();
    }, { once: true });
    v.addEventListener('error', () => {
      reject(new Error('video error code=' + (v.error && v.error.code)));
    }, { once: true });
    setTimeout(() => reject(new Error('loadedmetadata timeout ns=' + v.networkState + ' rs=' + v.readyState)), 90000);
  });
  try {
    await Promise.race([
      v.play(),
      sleep(2000)
    ]);
  } catch (_) {}
  snap('playing');
  window.__nj.phase = 'linear-wait';
  const linearOk = await waitCues(LINEAR_WAIT_MS);
  snap(linearOk ? 'linear:cues-ok' : 'linear:cues-fail');
  if (!linearOk) {
    window.__nj.phase = 'fail-linear';
    window.__nj.error = 'no cues during linear play';
    return;
  }
  window.__nj.phase = 'seek';
  v.currentTime = SEEK_S;
  await new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (!v.seeking && Math.abs(v.currentTime - SEEK_S) < 1.0) return resolve();
      if (performance.now() - t0 > 20000) return resolve();
      setTimeout(tick, 50);
    };
    v.addEventListener('seeked', () => resolve(), { once: true });
    tick();
  });
  snap('seeked');
  if (REASSERT === 'teardown') {
    window.__nj.phase = 'reassert';
    await reassertTeardown(v);
  } else {
    snap('reassert:none');
  }
  window.__nj.phase = 'poll';
  const pollUntil = performance.now() + AFTER_SEEK_MS;
  while (performance.now() < pollUntil) {
    snap('poll');
    if (coveringActive(v)) {
      snap('pass:cover');
      window.__nj.phase = 'pass';
      return;
    }
    await sleep(POLL_MS);
  }
  window.__nj.phase = 'fail-seek';
  window.__nj.error = 'no covering active cue within poll window';
  snap('fail');
}
run().catch((e) => {
  window.__nj.phase = 'error';
  window.__nj.error = String(e && e.message ? e.message : e);
  snap('error');
});
</script>
</body></html>`;
}

async function proxyRequest(upstreamPath, res) {
  const url = `${BASE}${upstreamPath}`;
  try {
    const upstream = await fetch(url);
    const buf = Buffer.from(await upstream.arrayBuffer());
    const ct = upstream.headers.get("content-type") || "application/octet-stream";
    res.writeHead(upstream.status, {
      "content-type": ct,
      "cache-control": "no-store",
      "access-control-allow-origin": "*"
    });
    res.end(buf);
  } catch (e) {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end(String(e));
  }
}

async function main() {
  console.log(
    `config base=${BASE} item=${ITEM} startMs=${START_MS} seekToMs=${SEEK_TO_MS} reassert=${REASSERT}`
  );

  const { webkit } = ensurePlaywright();

  // Session only after browser tooling is ready (avoids idle reap during download).
  const started = await httpJson(
    "POST",
    `${BASE}/api/v0/items/${ITEM}/sessions?startMs=${START_MS}`
  );
  if (![200, 201, 202].includes(started.status)) {
    console.log(`FAIL session POST status=${started.status}`, started.body);
    process.exit(2);
  }
  const sessionId = started.body.sessionId;
  const playlistPath = started.body.playlistUrl; // /api/v0/sessions/sN/master.m3u8
  const masterUrl = `${BASE}${playlistPath}`;
  console.log(`session ${sessionId} master=${masterUrl}`);

  if (!(await waitOk(masterUrl, 180))) {
    console.log("FAIL master never ready");
    process.exit(2);
  }
  const master = await httpText(masterUrl);
  const masterText = master.body.toString("utf8");
  const subRel = findSubUri(masterText);
  if (!subRel) {
    console.log("FAIL master has no SUBTITLES MEDIA");
    console.log(masterText.slice(0, 500));
    process.exit(2);
  }
  const masterDir = masterUrl.replace(/\/[^/]*$/, "");
  const subPlaylist = `${masterDir}/${subRel}`;
  console.log(`sub playlist ${subPlaylist}`);
  await waitOk(`${masterDir}/index.m3u8`, 30);
  if (!(await waitOk(subPlaylist, 60))) {
    console.log("FAIL subtitle playlist never ready");
    process.exit(2);
  }
  const subPl = await httpText(subPlaylist);
  const subPlText = subPl.body.toString("utf8");
  const segIdx = Math.floor(START_MS / 2000);
  const want = `seg${String(segIdx).padStart(3, "0")}.vtt`;
  let segRel = subPlText
    .split("\n")
    .map((l) => l.trim())
    .find((l) => l.endsWith(want));
  if (!segRel) {
    const m = subPlText.match(/(\S+seg\d+\.vtt)/);
    if (!m) {
      console.log("FAIL no seg in subtitle playlist");
      process.exit(2);
    }
    segRel = m[1];
  }
  const subsDir = subPlaylist.replace(/\/[^/]*$/, "");
  const segUrl = `${subsDir}/${segRel}`;
  console.log(`curl seg ${segUrl}`);
  const seg = await httpText(segUrl);
  const segText = seg.body.toString("utf8");
  console.log(
    `curl seg status=${seg.status} content-type=${seg.contentType} bytes=${seg.body.length}`
  );
  if (seg.status !== 200 || !segText.includes("WEBVTT") || !segText.includes("-->")) {
    console.log("FAIL subtitle segment empty or not WebVTT");
    console.log(segText.slice(0, 300));
    process.exit(2);
  }
  console.log(`curl seg ok preview=${JSON.stringify(segText.split("\n").slice(0, 6))}`);

  const sessionPrefix = playlistPath.replace(/\/master\.m3u8$/, "");
  const proxyMaster = `http://127.0.0.1:${HARNESS_PORT}${sessionPrefix}/master.m3u8`;
  const html = harnessHtml(proxyMaster, START_MS / 1000, SEEK_TO_MS / 1000);

  const server = createServer((req, res) => {
    const url = req.url || "/";
    if (url.startsWith("/play")) {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(html);
      return;
    }
    if (url.startsWith("/api/")) {
      void proxyRequest(url, res);
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise((r) => server.listen(HARNESS_PORT, "127.0.0.1", r));

  const browser = await webkit.launch({ headless: true });
  const page = await browser.newPage();
  page.on("console", (msg) => {
    if (msg.text().includes("nj")) console.log("browser:", msg.text());
  });
  try {
    await page.goto(`http://127.0.0.1:${HARNESS_PORT}/play`, {
      waitUntil: "domcontentloaded",
      timeout: 60000
    });
    const deadline = Date.now() + (LINEAR_WAIT_S + AFTER_SEEK_S + 120) * 1000;
    let state = null;
    while (Date.now() < deadline) {
      state = await page.evaluate(() => window.__nj);
      if (["pass", "fail-seek", "fail-linear", "error"].includes(state?.phase)) break;
      await sleep(500);
    }
    if (!state) state = await page.evaluate(() => window.__nj);

    const samples = state.samples || [];
    console.log("--- TRACE ---");
    for (const s of samples) {
      // Keep poll lines sparse: print non-poll always; poll every ~1s worth.
      if (s.label === "poll" && s.t % 1000 > POLL_MS) continue;
      const tracks = s.tracks || [];
      const tsummary =
        tracks
          .map(
            (t) =>
              `#${t.i} ${t.label} mode=${t.mode} cues=${t.cues} active=${t.active} cover=${t.cover}`
          )
          .join(" | ") || "no tracks";
      console.log(
        `t=${String(s.t).padStart(6)} ${String(s.label).padEnd(24)} ct=${String(s.currentTime).padEnd(8)} rs=${s.readyState} ns=${s.networkState ?? "?"} seeking=${s.seeking} [${tsummary}]`
      );
    }
    console.log("--- END TRACE ---");
    const passed = state.phase === "pass";
    console.log(
      `RESULT ${passed ? "PASS" : "FAIL"} phase=${state.phase} error=${state.error} reassert=${REASSERT}`
    );
    process.exitCode = passed ? 0 : 1;
  } finally {
    await browser.close();
    server.close();
    try {
      await fetch(`${BASE}/api/v0/sessions/${sessionId}`, { method: "DELETE" });
    } catch {
      /* ignore */
    }
  }
}

main().catch((e) => {
  console.log(`FAIL exception: ${e}`);
  process.exit(2);
});
