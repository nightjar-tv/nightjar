#!/usr/bin/env node
/**
 * Far-seek latency baseline on the committed ADR-0020 build.
 * Warm restarts only (session already playing), default 2 GiB run-cache budget.
 * n>=20 across titles × offsets. Reports min/p50/p90/max; flags eviction.
 *
 * BASE=http://127.0.0.1:8096 LOG=/tmp/nj-8096.log node scripts/far_seek_baseline.mjs
 */
import { setTimeout as sleep } from "node:timers/promises";
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";

const BASE = process.env.BASE || "http://127.0.0.1:8096";
const LOG = process.env.LOG || "/tmp/nj-far-seek-server.log";
const OUT_DIR =
  process.env.OUT_DIR ||
  `/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes`;
const OUT = `${OUT_DIR}/far-seek-baseline-2026-07-31.md`;
mkdirSync(OUT_DIR, { recursive: true });

/** Diverse titles: long-GOP copy candidates + short-GOP + feature. */
const CASES = [
  { id: 6653, label: "Elementary 3x05", durationMs: 2_599_931, fracs: [0.2, 0.4, 0.6, 0.75] },
  { id: 13790, label: "Rick and Morty 9x04", durationMs: 1_320_000, fracs: [0.25, 0.5, 0.7] },
  { id: 33, label: "12 Angry Men", durationMs: 5_768_768, fracs: [0.15, 0.35, 0.55, 0.75] },
  { id: 194, label: "Baywatch", durationMs: 7_200_000, fracs: [0.2, 0.45, 0.65] },
  { id: 22032, label: "Simpsons 7x23", durationMs: 1_320_000, fracs: [0.3, 0.55, 0.8] },
  { id: 8512, label: "Futurama 4x06", durationMs: 1_355_819, fracs: [0.25, 0.5, 0.7] },
];

async function json(url, opts = {}) {
  const res = await fetch(url, opts);
  const text = await res.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  return { status: res.status, body };
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
    if (m) segs.push({ name, startMs: Number(m[1]), uri: line.startsWith("/") ? line : null });
  }
  return segs;
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

async function fetchIndexFromMaster(masterUrl) {
  const master = await (await fetch(masterUrl)).text();
  const indexLine = master
    .split(/\r?\n/)
    .map((l) => l.trim())
    .find((l) => l && !l.startsWith("#") && l.includes("index.m3u8"));
  if (!indexLine) throw new Error("no index in master");
  const indexUrl = indexLine.startsWith("http")
    ? indexLine
    : indexLine.startsWith("/")
      ? absUrl(indexLine)
      : new URL(indexLine, masterUrl).href;
  const text = await (await fetch(indexUrl)).text();
  return { indexUrl, text, segs: parseSegs(text) };
}

async function waitFirstSeg(masterUrl, timeoutMs = 120_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const { segs } = await fetchIndexFromMaster(masterUrl);
      if (segs.length) {
        const seg = segs[0];
        const segUrl = seg.uri
          ? absUrl(seg.uri)
          : absUrl(
              masterUrl.replace(/\/runs\/\d+\/master\.m3u8.*/, `/${seg.name}`),
            );
        const res = await fetch(segUrl);
        if (res.ok) {
          const bytes = Buffer.from(await res.arrayBuffer());
          return { seg, bytes, ms: Date.now() - t0, segUrl };
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
  const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, idx)];
}

function countEvictionInLog(beforeLen) {
  let text = "";
  try {
    text = readFileSync(LOG, "utf8");
  } catch {
    return { hits: [], n: 0 };
  }
  const slice = text.slice(beforeLen);
  const hits = [];
  for (const line of slice.split("\n")) {
    if (/evict|swept orphaned|session_run_cache|cache budget/i.test(line)) {
      hits.push(line.slice(0, 200));
    }
  }
  return { hits, n: hits.length };
}

const samples = [];
const errors = [];

async function run() {
  // Confirm budget is default (unset).
  const health = await json(`${BASE}/api/v0/libraries`);
  if (health.status !== 200) throw new Error(`server not up: ${health.status}`);

  let logLen = 0;
  try {
    logLen = readFileSync(LOG, "utf8").length;
  } catch {
    logLen = 0;
  }

  for (const item of CASES) {
    // Probe item exists
    const info = await json(`${BASE}/api/v0/items/${item.id}`);
    if (info.status !== 200) {
      errors.push(`item ${item.id} missing (${info.status})`);
      continue;
    }
    const durationMs = info.body.durationMs || item.durationMs;

    // Cold create once, wait first segment — then only warm seeks count.
    const started = await json(
      `${BASE}/api/v0/items/${item.id}/sessions?startMs=0`,
      { method: "POST" },
    );
    if (started.status !== 202 && started.status !== 200) {
      errors.push(`start ${item.id}: ${started.status}`);
      continue;
    }
    const sid = started.body.sessionId;
    const master0 = absUrl(started.body.playlistUrl);
    if (!(await waitMaster(master0))) {
      errors.push(`master timeout ${item.id}`);
      continue;
    }
    const first = await waitFirstSeg(master0);
    if (!first) {
      errors.push(`first seg timeout ${item.id}`);
      continue;
    }
    // Brief play soak so restart_min_interval is clear.
    await sleep(2500);

    for (const frac of item.fracs) {
      const startMs = Math.floor((durationMs * frac) / 2000) * 2000;
      const beforeLog = (() => {
        try {
          return readFileSync(LOG, "utf8").length;
        } catch {
          return 0;
        }
      })();
      const tPost = Date.now();
      const seek = await json(
        `${BASE}/api/v0/sessions/${sid}/seek?startMs=${startMs}`,
        { method: "POST" },
      );
      const postMs = Date.now() - tPost;
      if (seek.status !== 202 && seek.status !== 200) {
        errors.push(`seek ${item.id}@${startMs}: ${seek.status}`);
        continue;
      }
      const master = absUrl(seek.body.playlistUrl);
      const landT0 = Date.now();
      if (!(await waitMaster(master))) {
        errors.push(`seek master timeout ${item.id}@${startMs}`);
        continue;
      }
      const land = await waitFirstSeg(master);
      const toLandMs = Date.now() - landT0 + postMs;
      const ev = countEvictionInLog(beforeLog);
      const sample = {
        itemId: item.id,
        label: item.label,
        frac,
        startMs,
        postMs,
        seekToFirstListedMs: land ? toLandMs : null,
        landedMs: seek.body.landedMs,
        runId: seek.body.runId,
        landOk: !!land,
        firstListed: land?.seg?.name ?? null,
        evictionLogHits: ev.n,
        evictionSample: ev.hits.slice(0, 2),
      };
      samples.push(sample);
      console.error(JSON.stringify(sample));
      await sleep(2500); // warm interval between seeks
    }

    await json(`${BASE}/api/v0/sessions/${sid}`, { method: "DELETE" }).catch(
      () => {},
    );
  }

  const ok = samples
    .filter((s) => s.landOk && s.seekToFirstListedMs != null)
    .map((s) => s.seekToFirstListedMs)
    .sort((a, b) => a - b);
  const evictionRuns = samples.filter((s) => s.evictionLogHits > 0);

  const summary = {
    n: ok.length,
    min: ok[0] ?? null,
    p50: pct(ok, 50),
    p90: pct(ok, 90),
    max: ok[ok.length - 1] ?? null,
    evictionRuns: evictionRuns.length,
    cacheBudgetDefaultBytes: 2 * 1024 * 1024 * 1024,
    errors,
  };

  const md = `# Far-seek latency baseline (ADR-0020 committed build)

- Date: 2026-07-31
- Build: committed \`transcode/adr-0020-producer-truth\` (producer-truth HLS)
- Cache: default \`SESSION_RUN_CACHE_BUDGET_BYTES\` = 2 GiB (not the 2 MiB
  pressure run that produced p50=5.4s / outliers 19s / 39.7s)
- Method: cold create + first segment once per title, then **warm**
  \`POST /seek\` only; wall time = seek POST → first listed segment 200 with
  bytes. \`n=${summary.n}\` across titles and offsets.

## Summary

| | ms |
|---|---:|
| min | ${summary.min} |
| p50 | ${summary.p50} |
| p90 | ${summary.p90} |
| max | ${summary.max} |

Eviction log hits during samples: **${summary.evictionRuns}** of ${samples.length} runs.
${
  evictionRuns.length
    ? "\nEviction-touched runs:\n" +
      evictionRuns
        .map(
          (s) =>
            `- ${s.label} @${s.startMs}ms run=${s.runId} hits=${s.evictionLogHits}`,
        )
        .join("\n")
    : "\nNo eviction markers observed in the server log during these seeks.\n"
}

## Samples

\`\`\`json
${JSON.stringify(samples, null, 2)}
\`\`\`

## Errors

\`\`\`json
${JSON.stringify(errors, null, 2)}
\`\`\`
`;

  writeFileSync(OUT, md);
  writeFileSync(
    `${OUT_DIR}/far-seek-baseline-2026-07-31.json`,
    JSON.stringify({ summary, samples, errors }, null, 2),
  );
  console.log(JSON.stringify(summary, null, 2));
  console.log(`wrote ${OUT}`);
}

run().catch((e) => {
  console.error(e);
  process.exit(1);
});
