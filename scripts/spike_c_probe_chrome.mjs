#!/usr/bin/env node
/**
 * Spike C — Chromium + hls.js probe against the local splice playlist.
 * Env: SPIKE_BASE SPIKE_MASTER SPIKE_SPLICE_S SPIKE_OUT SPIKE_CDP_PORT
 */
import { spawn } from "node:child_process";
import { writeFileSync, rmSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BASE = (process.env.SPIKE_BASE || "http://127.0.0.1:19641").replace(/\/$/, "");
const MASTER = process.env.SPIKE_MASTER || "master_nodisc.m3u8";
const SPLICE_S = Number(process.env.SPIKE_SPLICE_S || "6");
const OUT = process.env.SPIKE_OUT || "/tmp/spike_c_chrome.json";
const cdpPort = Number(process.env.SPIKE_CDP_PORT || "19651");
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const profile = `/tmp/spike-c-chrome-${process.pid}`;
const pageUrl = `${BASE}/index.html?engine=hlsjs&master=${encodeURIComponent(MASTER)}&spliceS=${SPLICE_S}`;

const chromeProc = spawn(
  chrome,
  [
    `--remote-debugging-port=${cdpPort}`,
    "--headless=new",
    "--disable-gpu",
    "--autoplay-policy=no-user-gesture-required",
    "--no-first-run",
    "--no-default-browser-check",
    `--user-data-dir=${profile}`,
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
      version = await fetch(`http://127.0.0.1:${cdpPort}/json/version`).then((r) =>
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

  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", {
    targetId,
    flatten: true,
  });
  await send("Page.enable", {}, sessionId);
  await send("Runtime.enable", {}, sessionId);
  await send("Page.navigate", { url: pageUrl }, sessionId);
  await onceMessage(
    ws,
    (m) =>
      m.method === "Page.loadEventFired" &&
      (!m.sessionId || m.sessionId === sessionId),
    30000,
  );
  await sleep(400);

  const evalRemote = async (expression) => {
    const result = await send(
      "Runtime.evaluate",
      {
        expression,
        awaitPromise: true,
        returnByValue: true,
      },
      sessionId,
    );
    if (result.exceptionDetails) {
      throw new Error(JSON.stringify(result.exceptionDetails));
    }
    return result.result.value;
  };

  const result = await evalRemote("window.__SPIKE.run()");
  const payload = {
    consumer: "chrome_hlsjs",
    variant: MASTER.includes("disc") && !MASTER.includes("nodisc") ? "disc" : "nodisc",
    master: MASTER,
    ...result,
  };
  writeFileSync(OUT, JSON.stringify(payload, null, 2) + "\n");
  console.error(
    `chrome ${payload.variant}: crossed=${payload.crossed} uninterrupted=${payload.uninterrupted} seekBack=${payload.seekBackOk}`,
  );

  chromeProc.kill("SIGTERM");
  try {
    rmSync(profile, { recursive: true, force: true });
  } catch {
    /* ignore */
  }
  process.exit(payload.crossed ? 0 : 2);
}

main().catch((e) => {
  writeFileSync(
    OUT,
    JSON.stringify(
      {
        consumer: "chrome_hlsjs",
        variant: MASTER.includes("nodisc") ? "nodisc" : "disc",
        error: String(e),
      },
      null,
      2,
    ) + "\n",
  );
  chromeProc.kill("SIGTERM");
  process.exit(1);
});
