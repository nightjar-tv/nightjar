#!/usr/bin/env node
// Drive system Chrome via CDP (no Playwright download). Play + seek Gate 1 check.
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const base = process.env.BASE_URL || "http://127.0.0.1:18122";
const itemId = process.env.ITEM_ID || "1";
const url = `${base}/items/${itemId}`;
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 19222;

const chromeProc = spawn(
  chrome,
  [
    `--remote-debugging-port=${port}`,
    "--headless=new",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    `--user-data-dir=/tmp/nightjar-chrome-cdp-${process.pid}`,
    "about:blank",
  ],
  { stdio: "ignore" },
);

function onceMessage(ws, pred, timeoutMs = 15000) {
  return new Promise((resolve, reject) => {
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
}

async function main() {
  // Cold Chrome on macOS can take >10s (updater + first headless profile).
  let version;
  for (let i = 0; i < 150; i++) {
    try {
      version = await fetch(`http://127.0.0.1:${port}/json/version`).then((r) =>
        r.json(),
      );
      break;
    } catch {
      await sleep(200);
    }
  }
  if (!version) throw new Error("Chrome CDP not up after 30s");

  const ws = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((res, rej) => {
    ws.addEventListener("open", res);
    ws.addEventListener("error", rej);
  });

  let id = 0;
  const send = (method, params = {}, sessionId) => {
    const msg = { id: ++id, method, params };
    if (sessionId) msg.sessionId = sessionId;
    ws.send(JSON.stringify(msg));
    return onceMessage(ws, (m) => m.id === msg.id).then((m) => {
      if (m.error) throw new Error(JSON.stringify(m.error));
      return m.result;
    });
  };

  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", {
    targetId,
    flatten: true,
  });
  await send("Page.enable", {}, sessionId);
  await send("Runtime.enable", {}, sessionId);
  await send("Page.navigate", { url }, sessionId);
  await onceMessage(
    ws,
    (m) =>
      m.method === "Page.loadEventFired" &&
      (!m.sessionId || m.sessionId === sessionId),
    20000,
  );
  await sleep(500);

  const { result } = await send(
    "Runtime.evaluate",
    {
      awaitPromise: true,
      returnByValue: true,
      expression: `(() => new Promise(async (resolve) => {
        const v = document.querySelector('video');
        if (!v) return resolve({ ok:false, reason:'no video' });
        v.muted = true;
        try { await v.play(); } catch (e) { return resolve({ ok:false, reason:'play: '+e }); }
        const t0 = performance.now();
        const waitPlay = () => new Promise((r) => {
          const tick = () => {
            if (v.currentTime > 0.05 && v.readyState >= 2) return r({ok:true, waited: performance.now()-t0});
            if (performance.now()-t0 > 8000) return r({ok:false, reason:'no progress rs='+v.readyState+' ns='+v.networkState+' err='+(v.error&&v.error.code)});
            requestAnimationFrame(tick);
          }; tick();
        });
        const started = await waitPlay();
        if (!started.ok) return resolve(started);
        const dur = v.duration;
        if (!Number.isFinite(dur) || dur < 2) return resolve({ok:false, reason:'bad duration '+dur});
        const target = Math.min(dur * 0.5, dur - 0.5);
        v.currentTime = target;
        const t1 = performance.now();
        const sought = await new Promise((r) => {
          const tick = () => {
            if (Math.abs(v.currentTime - target) < 0.75 && !v.seeking) return r({ok:true, at:v.currentTime, waited: performance.now()-t1});
            if (performance.now()-t1 > 8000) return r({ok:false, reason:'seek stall at '+v.currentTime});
            requestAnimationFrame(tick);
          }; tick();
        });
        if (!sought.ok) return resolve(sought);
        resolve({ ok:true, play_ms: Math.round(started.waited), seek_ms: Math.round(sought.waited), duration: +dur.toFixed(2), at: +sought.at.toFixed(2) });
      }))()`,
    },
    sessionId,
  );

  console.log(`chrome: ${JSON.stringify(result.value)}`);
  ws.close();
  chromeProc.kill("SIGKILL");
  if (!result.value?.ok) process.exit(1);
}

main().catch((e) => {
  console.error(`chrome: ${JSON.stringify({ ok: false, reason: String(e) })}`);
  try {
    chromeProc.kill("SIGKILL");
  } catch {}
  process.exit(1);
});
