#!/usr/bin/env node
/**
 * Exp 1 + 3 harness (Chrome/hls.js): API session + attach at land|first|two.
 * Does not use the Svelte page — isolates attach policy from UI.
 *
 * BASE=http://127.0.0.1:8096 ITEM=1733 START_MS=120000 TRACK=e2 ATTACH=first \
 *   node scripts/latency_attach_harness.mjs
 */
import { spawn } from "node:child_process";
import { writeFileSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { createServer } from "node:http";

const BASE = process.env.BASE || "http://127.0.0.1:8096";
const ITEM = process.env.ITEM || "1733";
const START_MS = Number(process.env.START_MS || "120000");
const TRACK = process.env.TRACK || "e2";
const ATTACH = process.env.ATTACH || "land"; // land|first|two
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const cdpPort = Number(process.env.CDP_PORT || "19444");

const landIdx = Math.floor(START_MS / 2000);
const windowIdx = Math.max(0, landIdx - 8);

async function waitMaster(masterUrl, label, t0, marks) {
  for (;;) {
    const res = await fetch(masterUrl);
    if (res.ok) {
      marks.push({ phase: label, ms: Date.now() - t0, status: res.status });
      await res.text();
      return true;
    }
    if (res.status === 404) return false;
    await sleep(100);
  }
}

async function main() {
  const t0 = Date.now();
  const marks = [];
  marks.push({ phase: "switch_requested", ms: 0 });

  const postUrl = `${BASE}/api/v0/items/${ITEM}/sessions?startMs=${START_MS}&audioTrackId=${TRACK}`;
  const started = await fetch(postUrl, { method: "POST" }).then((r) => {
    if (!r.ok) throw new Error(`POST ${r.status}`);
    return r.json();
  });
  marks.push({
    phase: "session_post_ok",
    ms: Date.now() - t0,
    sessionId: started.sessionId,
  });

  // Master ready (= map has at least one segment). ADR-0020: do not invent
  // segment URLs from playhead / SEGMENT_MS.
  const masterUrl = `${BASE}${started.playlistUrl}`;
  if (!(await waitMaster(masterUrl, "master_ready", t0, marks))) {
    throw new Error("master 404");
  }
  marks.push({
    phase: "attach_gate_open",
    ms: Date.now() - t0,
    mode: ATTACH,
    note: "master-only gate (ADR-0020)",
    windowIdx,
    landIdx,
  });
  const attachAt = Date.now() - t0;

  // Minimal page with hls.js from the nightjar embed if available, else CDN.
  const playlist = masterUrl;
  const startAt = START_MS / 1000;
  const html = `<!doctype html><html><body>
<video id="v" controls playsinline muted></video>
<script src="https://cdn.jsdelivr.net/npm/hls.js@1.5.17/dist/hls.min.js"><\/script>
<script>
const t0 = performance.now();
const marks = [];
const reqs = [];
const mark = (p, d) => {
  const ms = Math.round(performance.now() - t0);
  marks.push({ phase: p, ms, detail: d });
  console.info('[nj-probe]', ms, p, d || '');
};
const v = document.getElementById('v');
const origFetch = window.fetch.bind(window);
window.fetch = async (input, init) => {
  const url = typeof input === 'string' ? input : input.url;
  const t = Math.round(performance.now() - t0);
  const res = await origFetch(input, init);
  if (url.includes('/sessions/')) {
    const resource = url.split('/').pop().split('?')[0];
    reqs.push({ ms: t, status: res.status, resource });
    console.info('[nj-probe]', t, 'req', res.status, resource);
  }
  return res;
};
v.addEventListener('loadedmetadata', () => mark('loadedmetadata', 't=' + v.currentTime), { once: true });
v.addEventListener('canplay', () => mark('canplay', 't=' + v.currentTime), { once: true });
v.addEventListener('playing', () => mark('playing', 't=' + v.currentTime), { once: true });
if (v.requestVideoFrameCallback) {
  v.requestVideoFrameCallback(() => mark('first_decoded_frame', 't=' + v.currentTime));
}
mark('attach', '${ATTACH}');
const hls = new Hls({ startPosition: ${startAt}, enableWorker: true, maxBufferHole: 1.5 });
hls.on(Hls.Events.ERROR, (_, data) => {
  if (data.fatal) console.info('[nj-probe]', Math.round(performance.now()-t0), 'fatal', data.type, data.details);
});
hls.loadSource('${playlist}');
hls.attachMedia(v);
v.play().catch(() => {});
window.__NJ = () => ({ marks, reqs, currentTime: v.currentTime, paused: v.paused, readyState: v.readyState });
<\/script></body></html>`;

  const server = createServer((req, res) => {
    res.writeHead(200, { "content-type": "text/html" });
    res.end(html);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const pagePort = server.address().port;
  const pageUrl = `http://127.0.0.1:${pagePort}/`;

  const chromeProc = spawn(
    chrome,
    [
      `--remote-debugging-port=${cdpPort}`,
      "--headless=new",
      "--disable-gpu",
      "--disable-web-security",
      "--disable-features=IsolateOrigins,site-per-process",
      "--autoplay-policy=no-user-gesture-required",
      "--no-first-run",
      "--no-default-browser-check",
      `--user-data-dir=/tmp/nj-attach-chrome-${process.pid}`,
      "about:blank",
    ],
    { stdio: "ignore" },
  );

  let version;
  for (let i = 0; i < 150; i++) {
    try {
      version = await fetch(`http://127.0.0.1:${cdpPort}/json/version`).then((r) =>
        r.json(),
      );
      break;
    } catch {
      await sleep(200);
    }
  }
  if (!version) throw new Error("CDP not up");

  const ws = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((res, rej) => {
    ws.addEventListener("open", res);
    ws.addEventListener("error", rej);
  });
  let id = 0;
  const onceMessage = (pred, timeoutMs = 60000) =>
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
  const send = (method, params = {}, sessionId) => {
    const msg = { id: ++id, method, params };
    if (sessionId) msg.sessionId = sessionId;
    ws.send(JSON.stringify(msg));
    return onceMessage((m) => m.id === msg.id).then((m) => {
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
      if (String(text).includes("[nj-probe]")) consoleLines.push(String(text));
    }
  });

  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", {
    targetId,
    flatten: true,
  });
  await send("Page.enable", {}, sessionId);
  await send("Runtime.enable", {}, sessionId);
  await send("Page.navigate", { url: pageUrl }, sessionId);
  // Allow buffer build + 503 retries on cooking segments.
  await sleep(25000);

  const snap = await send(
    "Runtime.evaluate",
    { expression: "window.__NJ ? window.__NJ() : null", returnByValue: true },
    sessionId,
  );

  const client = snap?.result?.value;
  const intended = START_MS / 1000;
  const actual = client?.currentTime ?? null;
  const rewind = actual != null ? actual < intended - 2 : null;

  const out = {
    attach: ATTACH,
    startMs: START_MS,
    windowIdx,
    landIdx,
    serverMarks: marks,
    client,
    consoleLines,
    rewind,
    deltaFromLandSec:
      actual != null ? Number((actual - intended).toFixed(3)) : null,
    totalToAttachGateMs: attachAt,
    totalToFirstFrameMs: client?.marks?.find((m) => m.phase === "first_decoded_frame")
      ?.ms,
  };
  // Merge wall clock: attach_gate + client first frame
  if (out.totalToFirstFrameMs != null) {
    out.wallToFirstFrameMs = attachAt + out.totalToFirstFrameMs;
  }

  writeFileSync(`/tmp/nj-attach-${ATTACH}.json`, JSON.stringify(out, null, 2));
  console.log(JSON.stringify(out, null, 2));

  await fetch(`${BASE}/api/v0/sessions/${started.sessionId}`, { method: "DELETE" });
  chromeProc.kill();
  server.close();
  process.exit(0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
