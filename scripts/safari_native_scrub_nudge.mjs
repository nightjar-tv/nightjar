#!/usr/bin/env node
/**
 * WebKit native-HLS scrub nudge harness.
 *
 * Builds a fixed short fMP4 VOD, serves it with a controlled "cook" delay on
 * the final land segment, and compares late vs early currentTime nudge.
 *
 * Pass gate (wedge, not GET counts): after the fake cook ends and the final
 * land segment is servable, video.currentTime must advance past that land
 * on its own for a sustained window — without further user seeks.
 *
 * Finding (Playwright WebKit, 2026-07-28): late and early both PASS under
 * abandoned 503 or 404. This harness does **not** reproduce tonight's
 * real-Safari stall, so it cannot green-light or reject early-nudge alone.
 * Use it to catch regressions in the scripted sequence; confirm wedge fixes
 * on real Safari once a change is plausible.
 *
 *   NUDGE=late|early COOK_MS=3000 ABANDON_STATUS=503|404 \
 *     node scripts/safari_native_scrub_nudge.mjs
 *
 * Playwright installs into scripts/.tools (gitignored).
 */
import { createServer } from "node:http";
import { setTimeout as sleep } from "node:timers/promises";
import { spawnSync, execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import {
  mkdirSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const VENDOR = join(__dirname, ".tools");

const NUDGE = (process.env.NUDGE || "late").toLowerCase();
const COOK_MS = Number(process.env.COOK_MS || "3000");
const PRIOR_S = Number(process.env.PRIOR_S || "8");
const FINAL_S = Number(process.env.FINAL_S || "20");
const ADVANCE_S = Number(process.env.ADVANCE_S || "4");
const HARNESS_PORT = Number(process.env.HARNESS_PORT || "8766");
/** Status for abandoned prior-land URIs after commit (503 recoverable / 404 fatal). */
const ABANDON_STATUS = Number(process.env.ABANDON_STATUS || "503");
const SEGMENT_S = 2;
const DURATION_S = 40;

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

function buildVod(outDir) {
  mkdirSync(outDir, { recursive: true });
  const playlist = join(outDir, "index.m3u8");
  execFileSync(
    "ffmpeg",
    [
      "-y",
      "-hide_banner",
      "-loglevel",
      "error",
      "-f",
      "lavfi",
      "-i",
      `testsrc=duration=${DURATION_S}:size=320x240:rate=25`,
      "-f",
      "lavfi",
      "-i",
      `sine=frequency=440:duration=${DURATION_S}`,
      "-c:v",
      "libx264",
      "-pix_fmt",
      "yuv420p",
      "-g",
      "50",
      "-keyint_min",
      "50",
      "-sc_threshold",
      "0",
      "-force_key_frames",
      "expr:gte(t,n_forced*2)",
      "-c:a",
      "aac",
      "-b:a",
      "64k",
      "-f",
      "hls",
      "-hls_time",
      String(SEGMENT_S),
      "-hls_playlist_type",
      "vod",
      "-hls_segment_type",
      "fmp4",
      "-hls_fmp4_init_filename",
      "init.mp4",
      "-hls_segment_filename",
      join(outDir, "seg%03d.m4s"),
      playlist
    ],
    { stdio: "inherit" }
  );
  if (!existsSync(playlist) || !existsSync(join(outDir, "init.mp4"))) {
    throw new Error("ffmpeg did not produce HLS VOD");
  }
  const segs = readdirSync(outDir).filter((f) => /^seg\d+\.m4s$/.test(f));
  if (segs.length < FINAL_S / SEGMENT_S + 2) {
    throw new Error(`too few segments: ${segs.length}`);
  }
  return playlist;
}

function segmentIndexAtSeconds(seconds) {
  return Math.max(0, Math.floor(seconds / SEGMENT_S));
}

function harnessHtml(mediaUrl) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>nj scrub nudge</title></head>
<body>
<video id="v" controls playsinline muted></video>
<script>
window.__nj = {
  phase: 'boot',
  error: null,
  samples: [],
  gets: {},
  nudge: ${JSON.stringify(NUDGE)},
  priorS: ${PRIOR_S},
  finalS: ${FINAL_S},
  cookMs: ${COOK_MS},
  advanceS: ${ADVANCE_S},
  landReadyAt: null,
  advanceStart: null,
  advanceEnd: null,
  pass: false
};

const MEDIA = ${JSON.stringify(mediaUrl)};
const SEGMENT_S = ${SEGMENT_S};
const NUDGE = window.__nj.nudge;
const PRIOR_S = window.__nj.priorS;
const FINAL_S = window.__nj.finalS;
const ADVANCE_MS = Math.floor(window.__nj.advanceS * 1000);

function snap(label, extra) {
  const v = document.getElementById('v');
  window.__nj.samples.push(Object.assign({
    t: Math.round(performance.now()),
    label,
    currentTime: +v.currentTime.toFixed(3),
    seeking: v.seeking,
    paused: v.paused,
    readyState: v.readyState,
    networkState: v.networkState
  }, extra || {}));
}

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

function segUrl(idx) {
  const n = String(idx).padStart(3, '0');
  return MEDIA.replace(/index\\.m3u8.*$/, 'seg' + n + '.m4s');
}

function noteGet(name) {
  window.__nj.gets[name] = (window.__nj.gets[name] || 0) + 1;
}

async function ensureLand(idx, signal) {
  const url = segUrl(idx);
  while (!signal.aborted) {
    noteGet('ensure:' + idx);
    let status = 0;
    try {
      const res = await fetch(url, { signal, cache: 'no-store' });
      status = res.status;
      if (res.ok) return true;
    } catch (e) {
      if (signal.aborted) return false;
    }
    snap('ensure-poll', { idx, status });
    await sleep(200);
  }
  return false;
}

async function run() {
  const v = document.getElementById('v');
  const priorIdx = Math.floor(PRIOR_S / SEGMENT_S);
  const finalIdx = Math.floor(FINAL_S / SEGMENT_S);

  window.__nj.phase = 'attach';
  v.src = MEDIA;
  await new Promise((resolve, reject) => {
    v.addEventListener('loadedmetadata', () => { snap('loadedmetadata'); resolve(); }, { once: true });
    v.addEventListener('error', () => {
      reject(new Error('video error code=' + (v.error && v.error.code)));
    }, { once: true });
    setTimeout(() => reject(new Error('loadedmetadata timeout')), 60000);
  });

  try { await Promise.race([v.play(), sleep(2000)]); } catch (_) {}
  snap('playing');
  await sleep(1500);
  snap('linear');

  window.__nj.phase = 'prior';
  v.currentTime = PRIOR_S;
  await new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (!v.seeking && Math.abs(v.currentTime - PRIOR_S) < 0.75) return resolve();
      if (performance.now() - t0 > 15000) return resolve();
      setTimeout(tick, 50);
    };
    v.addEventListener('seeked', () => resolve(), { once: true });
    tick();
  });
  snap('prior-seeked');
  try { await v.play(); } catch (_) {}
  await sleep(800);

  window.__nj.phase = 'commit';
  await fetch('/harness/commit?finalIdx=' + finalIdx + '&priorIdx=' + priorIdx, { method: 'POST' });
  snap('commit', { nudge: NUDGE, finalS: FINAL_S, priorS: PRIOR_S });

  const ac = new AbortController();
  let holdTimer = null;
  if (NUDGE === 'early') {
    v.currentTime = FINAL_S;
    snap('early-nudge');
  } else {
    // Re-seek onto prior after abandon flips on, so native must re-fetch
    // (buffered prior would otherwise never hit abandoned URIs).
    v.currentTime = Math.max(0, PRIOR_S - 0.01);
    await sleep(50);
    v.currentTime = PRIOR_S;
    snap('late-hold');
    holdTimer = setInterval(() => {
      if (Math.abs(v.currentTime - PRIOR_S) > 0.35) {
        v.currentTime = PRIOR_S;
      }
    }, 200);
  }

  const ready = await ensureLand(finalIdx, ac.signal);
  if (holdTimer) clearInterval(holdTimer);
  snap(ready ? 'land-ready' : 'land-fail');
  if (!ready) {
    window.__nj.phase = 'fail-land';
    window.__nj.error = 'final land segment never 200';
    return;
  }
  window.__nj.landReadyAt = performance.now();

  if (NUDGE === 'late') {
    v.currentTime = FINAL_S;
    snap('late-nudge-after-ready');
  }
  try { await v.play(); } catch (_) {}
  snap('play-after-land');

  window.__nj.phase = 'advance-watch';
  const watchStart = performance.now();
  window.__nj.advanceStart = +v.currentTime.toFixed(3);
  let maxSeen = v.currentTime;
  let lastSample = watchStart;
  while (performance.now() - watchStart < ADVANCE_MS) {
    await sleep(250);
    const now = performance.now();
    if (now - lastSample >= 500) {
      snap('advance-sample');
      lastSample = now;
    }
    if (v.currentTime > maxSeen) maxSeen = v.currentTime;
  }
  window.__nj.advanceEnd = +v.currentTime.toFixed(3);
  const advanced = maxSeen >= FINAL_S + 1.0 && window.__nj.advanceEnd >= FINAL_S + 0.75;
  snap('advance-done', { maxSeen: +maxSeen.toFixed(3), advanced });

  if (advanced) {
    window.__nj.phase = 'pass';
    window.__nj.pass = true;
  } else {
    window.__nj.phase = 'fail-stall';
    window.__nj.error =
      'currentTime did not advance past land after cook (start=' +
      window.__nj.advanceStart +
      ' end=' +
      window.__nj.advanceEnd +
      ' max=' +
      maxSeen.toFixed(3) +
      ')';
  }
}

run().catch((e) => {
  window.__nj.phase = 'error';
  window.__nj.error = String(e && e.message ? e.message : e);
  snap('error');
});
</script>
</body></html>`;
}

async function main() {
  if (NUDGE !== "late" && NUDGE !== "early") {
    console.log(`FAIL NUDGE must be late|early, got ${NUDGE}`);
    process.exit(2);
  }
  console.log(
    `config nudge=${NUDGE} cookMs=${COOK_MS} priorS=${PRIOR_S} finalS=${FINAL_S} advanceS=${ADVANCE_S} abandonStatus=${ABANDON_STATUS}`
  );

  const { webkit } = ensurePlaywright();
  const vodDir = mkdtempSync(join(tmpdir(), "nj-scrub-nudge-"));
  console.log(`building vod in ${vodDir}`);
  try {
    buildVod(vodDir);
  } catch (e) {
    rmSync(vodDir, { recursive: true, force: true });
    throw e;
  }

  const priorIdx = segmentIndexAtSeconds(PRIOR_S);
  const finalIdx = segmentIndexAtSeconds(FINAL_S);
  const state = {
    committedAt: null,
    finalIdx,
    priorIdx,
    gets: {}
  };

  const mediaPath = "/vod/index.m3u8";
  const html = harnessHtml(`http://127.0.0.1:${HARNESS_PORT}${mediaPath}`);

  const server = createServer((req, res) => {
    const raw = req.url || "/";
    const url = new URL(raw, `http://127.0.0.1:${HARNESS_PORT}`);

    if (url.pathname === "/play") {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(html);
      return;
    }

    if (url.pathname === "/harness/commit" && req.method === "POST") {
      state.committedAt = Date.now();
      const fi = Number(url.searchParams.get("finalIdx"));
      const pi = Number(url.searchParams.get("priorIdx"));
      if (Number.isFinite(fi)) state.finalIdx = fi;
      if (Number.isFinite(pi)) state.priorIdx = pi;
      res.writeHead(204);
      res.end();
      return;
    }

    if (url.pathname === "/harness/stats") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(state));
      return;
    }

    if (url.pathname.startsWith("/vod/")) {
      const name = url.pathname.slice("/vod/".length);
      if (name.includes("..") || name.includes("/")) {
        res.writeHead(400);
        res.end();
        return;
      }
      const filePath = join(vodDir, name);
      if (!existsSync(filePath) || !statSync(filePath).isFile()) {
        res.writeHead(404);
        res.end();
        return;
      }

      state.gets[name] = (state.gets[name] || 0) + 1;

      const segMatch = /^seg(\d+)\.m4s$/.exec(name);
      if (segMatch && state.committedAt != null) {
        const idx = Number(segMatch[1]);
        const cookElapsed = Date.now() - state.committedAt;
        // Final land + short dig-back band: 503 until cook delay, then bytes.
        // Farther behind: abandoned for the cook (preempt hole).
        const digBack = 2;
        if (idx >= state.finalIdx - digBack && idx <= state.finalIdx + 2) {
          if (cookElapsed < COOK_MS) {
            res.writeHead(503, {
              "content-type": "text/plain",
              "cache-control": "no-store"
            });
            res.end("cooking");
            return;
          }
        } else if (idx < state.finalIdx - digBack) {
          res.writeHead(ABANDON_STATUS, {
            "content-type": "text/plain",
            "cache-control": "no-store"
          });
          res.end("abandoned");
          return;
        }
      }

      const buf = readFileSync(filePath);
      let ct = "application/octet-stream";
      if (name.endsWith(".m3u8")) ct = "application/vnd.apple.mpegurl";
      else if (name.endsWith(".mp4")) ct = "video/mp4";
      else if (name.endsWith(".m4s")) ct = "video/iso.segment";
      res.writeHead(200, { "content-type": ct, "cache-control": "no-store" });
      res.end(buf);
      return;
    }

    res.writeHead(404);
    res.end();
  });

  await new Promise((r) => server.listen(HARNESS_PORT, "127.0.0.1", r));
  console.log(`harness http://127.0.0.1:${HARNESS_PORT}/play`);

  const browser = await webkit.launch({ headless: true });
  const page = await browser.newPage();
  page.on("console", (msg) => {
    const t = msg.text();
    if (t.includes("nj") || t.includes("error")) console.log("browser:", t);
  });

  try {
    await page.goto(`http://127.0.0.1:${HARNESS_PORT}/play`, {
      waitUntil: "domcontentloaded",
      timeout: 60000
    });
    const deadline = Date.now() + COOK_MS + ADVANCE_S * 1000 + 90000;
    let nj = null;
    while (Date.now() < deadline) {
      nj = await page.evaluate(() => window.__nj);
      if (["pass", "fail-stall", "fail-land", "error"].includes(nj?.phase)) break;
      await sleep(400);
    }
    if (!nj) nj = await page.evaluate(() => window.__nj);

    console.log("--- SAMPLES ---");
    for (const s of nj.samples || []) {
      if (s.label === "ensure-poll" && s.t % 1000 > 200) continue;
      console.log(
        `t=${String(s.t).padStart(6)} ${String(s.label).padEnd(22)} ct=${String(s.currentTime).padEnd(8)} rs=${s.readyState} ns=${s.networkState} paused=${s.paused}${s.advanced != null ? ` advanced=${s.advanced}` : ""}`
      );
    }
    console.log("--- END SAMPLES ---");
    console.log("pageGets", JSON.stringify(nj.gets || {}));
    console.log("serverGets", JSON.stringify(state.gets));

    const passed = nj.phase === "pass" && nj.pass === true;
    console.log(
      `RESULT ${passed ? "PASS" : "FAIL"} nudge=${NUDGE} phase=${nj.phase} error=${nj.error || ""} advanceStart=${nj.advanceStart} advanceEnd=${nj.advanceEnd}`
    );
    if (!passed && NUDGE === "early") {
      console.log(
        "NOTE: early nudge still stalled in harness — problem may not be nudge-timing (native penalty-box / hard engine limit). That is a harness success: five-minute answer."
      );
    }
    if (!passed && NUDGE === "late") {
      console.log(
        "NOTE: late nudge stalled as expected under abandoned prior-land + withheld currentTime (models tonight's wedge)."
      );
    }
    process.exitCode = passed ? 0 : 1;
  } finally {
    await browser.close();
    server.close();
    rmSync(vodDir, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.log(`FAIL exception: ${e}`);
  process.exit(2);
});
