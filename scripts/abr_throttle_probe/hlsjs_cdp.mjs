#!/usr/bin/env node
// ABR throttle probe — hls.js via system Chrome CDP (no Playwright download).
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const pageUrl = process.env.PAGE_URL || "http://127.0.0.1:8765/page.html";
const out =
  process.env.OUT ||
  "notes/client-arch/abr-hlsjs-events.json";
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = Number(process.env.CDP_PORT || "19444");
const waitMs = Number(process.env.WAIT_MS || "48000");

const chromeProc = spawn(
  chrome,
  [
    `--remote-debugging-port=${port}`,
    "--headless=new",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    `--user-data-dir=/tmp/nightjar-abr-chrome-${process.pid}`,
    "about:blank",
  ],
  { stdio: "ignore" },
);

function onceMessage(ws, pred, timeoutMs = 20000) {
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

let nextId = 1;
async function send(ws, method, params = {}) {
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params }));
  const msg = await onceMessage(ws, (m) => m.id === id);
  if (msg.error) throw new Error(JSON.stringify(msg.error));
  return msg.result;
}

async function main() {
  await fetch("http://127.0.0.1:8765/reset").catch(() => {});
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

  const { targetId } = await send(ws, "Target.createTarget", { url: pageUrl });
  const { sessionId } = await send(ws, "Target.attachToTarget", {
    targetId,
    flatten: true,
  });

  async function session(method, params = {}) {
    const id = nextId++;
    ws.send(
      JSON.stringify({
        id,
        method: "Target.sendMessageToTarget",
        // flattened sessions use sessionId on the envelope in newer CDP;
        // Chrome headless with flatten:true accepts sessionId at top level.
        sessionId,
        params: undefined,
      }),
    );
    // Prefer flat sessionId form:
    ws.send(JSON.stringify({ id, method, params, sessionId }));
    const msg = await onceMessage(ws, (m) => m.id === id, 60000);
    if (msg.error) throw new Error(JSON.stringify(msg.error));
    return msg.result;
  }

  // Flat session protocol (Chrome):
  const flatSend = async (method, params = {}, timeoutMs = 60000) => {
    const id = nextId++;
    ws.send(JSON.stringify({ id, sessionId, method, params }));
    const msg = await onceMessage(ws, (m) => m.id === id, timeoutMs);
    if (msg.error) throw new Error(`${method}: ${JSON.stringify(msg.error)}`);
    return msg.result;
  };

  await flatSend("Page.enable");
  await flatSend("Runtime.enable");
  // Network conditions ~1 Mbps down (same order as throttle proxy).
  await flatSend("Network.enable");
  await flatSend("Network.emulateNetworkConditions", {
    offline: false,
    latency: 40,
    downloadThroughput: 125000, // bytes/s ≈ 1 Mbps
    uploadThroughput: 125000,
    connectionType: "cellular3g",
  });

  await flatSend("Page.navigate", { url: pageUrl });
  await sleep(2000);

  let events = [];
  const deadline = Date.now() + waitMs;
  while (Date.now() < deadline) {
    const r = await flatSend("Runtime.evaluate", {
      expression:
        "({ done: window.__abrDone === true, events: window.__abrEvents || [] })",
      returnByValue: true,
    });
    events = r.result.value.events || [];
    if (r.result.value.done) break;
    await sleep(1000);
  }

  writeFileSync(out, JSON.stringify(events, null, 2));
  const levels = events.filter((e) => e.kind === "level");
  const waits = events.filter((e) => e.kind === "waiting" || e.kind === "stalled");
  console.log(
    JSON.stringify(
      {
        ok: true,
        events: events.length,
        level_switches: levels,
        waiting_or_stalled: waits.length,
        stop: events.find((e) => e.kind === "stop") || null,
      },
      null,
      2,
    ),
  );

  ws.close();
  chromeProc.kill("SIGKILL");
}

main().catch((e) => {
  console.error(String(e));
  try {
    chromeProc.kill("SIGKILL");
  } catch {}
  process.exit(1);
});
