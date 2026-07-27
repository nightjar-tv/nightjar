#!/usr/bin/env node
/**
 * Experiment 1+3 (Chrome / hls.js): open item page with njAttach mode,
 * seek mid-title, switch audio, capture [nj-probe] console lines and
 * network-ish request marks from the probe.
 *
 * Usage:
 *   BASE_URL=http://127.0.0.1:8097 ITEM_ID=1733 ATTACH=land \
 *     node scripts/latency_exp1_chrome.mjs
 */
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { writeFileSync } from "node:fs";

const base = process.env.BASE_URL || "http://127.0.0.1:8097";
const itemId = process.env.ITEM_ID || "1733";
const attach = process.env.ATTACH || "land"; // land|first|two
const seekMs = Number(process.env.SEEK_MS || "120000"); // 2 min in
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = Number(process.env.CDP_PORT || "19333");
const url = `${base}/items/${itemId}?njProbe=1&njAttach=${attach}`;

const chromeProc = spawn(
  chrome,
  [
    `--remote-debugging-port=${port}`,
    "--headless=new",
    "--disable-gpu",
    "--autoplay-policy=no-user-gesture-required",
    "--no-first-run",
    "--no-default-browser-check",
    `--user-data-dir=/tmp/nightjar-lat-chrome-${process.pid}`,
    "about:blank",
  ],
  { stdio: "ignore" },
);

function onceMessage(ws, pred, timeoutMs = 60000) {
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
  if (!version) throw new Error("Chrome CDP not up");

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

  const consoleLines = [];
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", {
    targetId,
    flatten: true,
  });
  await send("Page.enable", {}, sessionId);
  await send("Runtime.enable", {}, sessionId);
  await send("Network.enable", {}, sessionId);

  ws.addEventListener("message", (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.method === "Runtime.consoleAPICalled" && msg.sessionId === sessionId) {
      const text = (msg.params.args || [])
        .map((a) => a.value ?? a.description ?? "")
        .join(" ");
      if (text.includes("[nj-probe")) consoleLines.push(text);
    }
  });

  await send("Page.navigate", { url }, sessionId);
  await onceMessage(
    ws,
    (m) =>
      m.method === "Page.loadEventFired" &&
      (m.sessionId === sessionId || !m.sessionId),
    60000,
  );
  await sleep(2000);

  // Wait until a video exists and (hopefully) is playing or has a playlist.
  for (let i = 0; i < 60; i++) {
    const st = await send(
      "Runtime.evaluate",
      {
        expression: `(() => {
          const v = document.querySelector('video');
          return v ? { ready: v.readyState, t: v.currentTime, err: document.body.innerText.includes('failed') } : null;
        })()`,
        returnByValue: true,
      },
      sessionId,
    );
    if (st?.result?.value && !st.result.value.err) break;
    await sleep(500);
  }

  // Seek via video currentTime + seeked handler (playlist?startMs=).
  await send(
    "Runtime.evaluate",
    {
      expression: `(() => {
        const v = document.querySelector('video');
        if (!v) return 'no-video';
        v.currentTime = ${seekMs / 1000};
        return 'seeked-to-${seekMs}';
      })()`,
      returnByValue: true,
    },
    sessionId,
  );
  await sleep(8000); // allow mid-title window cook

  // Click the non-selected audio radio (switch).
  const switchResult = await send(
    "Runtime.evaluate",
    {
      expression: `(() => {
        const inputs = [...document.querySelectorAll('input[name="audio-track"]')];
        if (inputs.length < 2) return { ok: false, reason: 'need-2-tracks', n: inputs.length };
        const next = inputs.find(i => !i.checked) || inputs[1];
        next.click();
        return { ok: true, value: next.value };
      })()`,
      returnByValue: true,
    },
    sessionId,
  );
  console.log("switch", JSON.stringify(switchResult?.result?.value));

  // Collect for up to 45s waiting for summary / resumed.
  const tEnd = Date.now() + 45000;
  while (Date.now() < tEnd) {
    if (consoleLines.some((l) => l.includes("[nj-probe-summary]"))) break;
    await sleep(500);
  }

  // Snapshot player state.
  const snap = await send(
    "Runtime.evaluate",
    {
      expression: `(() => {
        const v = document.querySelector('video');
        const preparing = [...document.querySelectorAll('[role="status"]')].map(e => e.textContent);
        return {
          currentTime: v?.currentTime ?? null,
          paused: v?.paused ?? null,
          readyState: v?.readyState ?? null,
          preparing,
        };
      })()`,
      returnByValue: true,
    },
    sessionId,
  );

  const out = {
    attach,
    seekMs,
    url,
    switch: switchResult?.result?.value,
    snap: snap?.result?.value,
    consoleLines,
  };
  const path = `/tmp/nj-lat-exp1-${attach}.json`;
  writeFileSync(path, JSON.stringify(out, null, 2));
  console.log(JSON.stringify(out, null, 2));
  console.log(`wrote ${path}`);

  chromeProc.kill();
  process.exit(0);
}

main().catch((e) => {
  console.error(e);
  chromeProc.kill();
  process.exit(1);
});
