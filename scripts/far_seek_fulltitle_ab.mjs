#!/usr/bin/env node
/**
 * Structural A/B for transcode far-seek (no server change).
 *
 * Path A — current ADR-0020 cook: warm POST /seek → new playlist URI →
 *          first listed land segment 200 (source-swap path).
 * Path B — full-title / cook-on-miss shape: land segment already on disk
 *          from a prior cook; time GET of that URI on the *original* run
 *          (no POST, no new URI, no re-attach). Isolates swap+API from
 *          encode. Pre-cook once, then measure B repeatedly.
 *
 * BASE=http://127.0.0.1:8096 ITEM=33 FRAC=0.55 node scripts/far_seek_fulltitle_ab.mjs
 */
import { setTimeout as sleep } from "node:timers/promises";
import { writeFileSync, mkdirSync } from "node:fs";

const BASE = process.env.BASE || "http://127.0.0.1:8096";
const ITEM = Number(process.env.ITEM || 33);
const FRAC = Number(process.env.FRAC || 0.55);
const N = Number(process.env.N || 5);
const OUT_DIR =
  process.env.OUT_DIR ||
  "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes";
const STAMP = process.env.STAMP || new Date().toISOString().slice(0, 10);

async function json(url, opts = {}) {
  const res = await fetch(url, opts);
  const text = await res.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  return { status: res.status, body, headers: res.headers };
}

function absUrl(path) {
  return path.startsWith("http") ? path : `${BASE}${path}`;
}

function parseSegs(text) {
  const segs = [];
  for (const line of text.split(/\r?\n/)) {
    if (!line || line.startsWith("#")) continue;
    const name = line.split("/").pop();
    const m = /^seg_(\d{11})\.m4s$/.exec(name);
    if (m) {
      segs.push({
        name,
        startMs: Number(m[1]),
        uri: line.startsWith("/") ? line : null,
      });
    }
  }
  return segs;
}

async function fetchIndex(masterUrl) {
  const master = await (await fetch(masterUrl)).text();
  const indexLine = master
    .split(/\r?\n/)
    .map((l) => l.trim())
    .find((l) => l && !l.startsWith("#") && l.includes("index.m3u8"));
  if (!indexLine) throw new Error("no index");
  const indexUrl = indexLine.startsWith("http")
    ? indexLine
    : indexLine.startsWith("/")
      ? absUrl(indexLine)
      : new URL(indexLine, masterUrl).href;
  const text = await (await fetch(indexUrl)).text();
  return { indexUrl, text, segs: parseSegs(text) };
}

async function waitMaster(url, timeoutMs = 90_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const res = await fetch(url);
    if (res.ok) return true;
    await sleep(100);
  }
  return false;
}

async function waitFirstSeg(masterUrl, timeoutMs = 120_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const { segs } = await fetchIndex(masterUrl);
      if (segs.length) {
        const seg = segs[0];
        const segUrl = seg.uri
          ? absUrl(seg.uri)
          : absUrl(
              masterUrl.replace(/\/runs\/\d+\/master\.m3u8.*/, `/${seg.name}`),
            );
        const res = await fetch(segUrl);
        if (res.ok) {
          await res.arrayBuffer();
          return { seg, ms: Date.now() - t0, segUrl };
        }
      }
    } catch {
      // retry
    }
    await sleep(150);
  }
  return null;
}

function pct(sorted, p) {
  if (!sorted.length) return null;
  const idx = Math.min(
    sorted.length - 1,
    Math.ceil((p / 100) * sorted.length) - 1,
  );
  return sorted[Math.max(0, idx)];
}

function stats(vals) {
  const s = [...vals].sort((a, b) => a - b);
  return { n: s.length, min: s[0] ?? null, p50: pct(s, 50), max: s[s.length - 1] ?? null };
}

async function pathA(sid, startMs) {
  const tPost = Date.now();
  const seek = await json(
    `${BASE}/api/v0/sessions/${sid}/seek?startMs=${startMs}`,
    { method: "POST" },
  );
  const postMs = Date.now() - tPost;
  if (seek.status !== 202 && seek.status !== 200) {
    return { ok: false, error: `seek ${seek.status}` };
  }
  const master = absUrl(seek.body.playlistUrl);
  const landT0 = Date.now();
  if (!(await waitMaster(master))) {
    return { ok: false, error: "master timeout" };
  }
  const land = await waitFirstSeg(master);
  return {
    ok: !!land,
    postMs,
    seekToFirstListedMs: land ? Date.now() - landT0 + postMs : null,
    landSeg: land?.seg?.name ?? null,
    playlistUrl: seek.body.playlistUrl,
    runId: seek.body.runId,
  };
}

async function pathB(segUrl) {
  const t0 = Date.now();
  const res = await fetch(segUrl);
  if (!res.ok) return { ok: false, error: `GET ${res.status}`, ms: Date.now() - t0 };
  await res.arrayBuffer();
  return { ok: true, ms: Date.now() - t0 };
}

async function main() {
  const info = await json(`${BASE}/api/v0/items/${ITEM}`);
  if (info.status !== 200) throw new Error(`item ${ITEM}: ${info.status}`);
  const method = info.body.playbackMethod;
  const durationMs = info.body.durationMs;
  const startMs = Math.floor((durationMs * FRAC) / 2000) * 2000;
  console.error(
    JSON.stringify({ ITEM, method, durationMs, FRAC, startMs, N }),
  );

  // Cold start at 0, wait first seg, soak.
  const started = await json(`${BASE}/api/v0/items/${ITEM}/sessions?startMs=0`, {
    method: "POST",
  });
  if (started.status !== 202 && started.status !== 200) {
    throw new Error(`start ${started.status}`);
  }
  const sid = started.body.sessionId;
  const master0 = absUrl(started.body.playlistUrl);
  if (!(await waitMaster(master0))) throw new Error("master0 timeout");
  if (!(await waitFirstSeg(master0))) throw new Error("first seg timeout");
  await sleep(2500);

  // Pre-cook land once via Path A (establishes mapped bytes for Path B).
  const precook = await pathA(sid, startMs);
  if (!precook.ok) throw new Error(`precook failed: ${precook.error}`);
  console.error(JSON.stringify({ phase: "precook", ...precook }));
  await sleep(2500);

  // Resolve a durable land URI: after precook, map may serve from session root.
  const landName = precook.landSeg;
  if (!landName) throw new Error("no land seg name");
  const landUrl = absUrl(`/api/v0/sessions/${sid}/${landName}`);
  const probe = await fetch(landUrl);
  if (!probe.ok) {
    throw new Error(
      `land URI not durable on session root: ${landUrl} → ${probe.status}`,
    );
  }
  await probe.arrayBuffer();

  const aSamples = [];
  const bSamples = [];

  for (let i = 0; i < N; i++) {
    // Path A: another far seek (force new run) — pick a nearby grid point
    // alternating so we still pay restart when possible.
    const offsetMs = startMs + (i % 2 === 0 ? 0 : 2000);
    const a = await pathA(sid, offsetMs);
    aSamples.push(a);
    console.error(JSON.stringify({ phase: "A", i, ...a }));
    await sleep(2500);

    const b = await pathB(landUrl);
    bSamples.push(b);
    console.error(JSON.stringify({ phase: "B", i, ...b }));
    await sleep(500);
  }

  await json(`${BASE}/api/v0/sessions/${sid}`, { method: "DELETE" }).catch(
    () => {},
  );

  const aOk = aSamples
    .filter((s) => s.ok && s.seekToFirstListedMs != null)
    .map((s) => s.seekToFirstListedMs);
  const bOk = bSamples.filter((s) => s.ok).map((s) => s.ms);
  const summary = {
    stamp: STAMP,
    itemId: ITEM,
    playbackMethod: method,
    frac: FRAC,
    startMs,
    pathA_postSeek_swap: stats(aOk),
    pathB_preCooked_get: stats(bOk),
    ratio_p50:
      aOk.length && bOk.length
        ? Number((pct([...aOk].sort((x, y) => x - y), 50) / pct([...bOk].sort((x, y) => x - y), 50)).toFixed(2))
        : null,
    note:
      "Path B is already-cooked land GET (no swap). If A/B p50 ≫ 1, swap+restart dominates over pure serve; compare A to latency_exp2 land_seg_ms for encode share.",
  };

  mkdirSync(OUT_DIR, { recursive: true });
  const out = `${OUT_DIR}/far-seek-fulltitle-ab-${STAMP}.json`;
  writeFileSync(
    out,
    JSON.stringify({ summary, aSamples, bSamples, precook }, null, 2),
  );
  console.log(JSON.stringify(summary, null, 2));
  console.log(`wrote ${out}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
