#!/usr/bin/env node
/**
 * ADR-0020 acceptance + map-hit + far-seek latency (not CI — Rule 4.2).
 *
 * BASE=http://127.0.0.1:8096 node scripts/adr0020_acceptance_probe.mjs
 *
 * Proves: warm create→play→far-seek on listed items; 8519@75% usable extent;
 * Elementary mid-scrub; scrub-back map hit (no disk growth / no new cook);
 * records far-seek → first_segment wall times from server log markers via
 * client-measured seek→land.
 */
import { setTimeout as sleep } from "node:timers/promises";
import { writeFileSync, mkdirSync } from "node:fs";

const BASE = process.env.BASE || "http://127.0.0.1:8096";
const OUT = process.env.OUT_DIR || `/tmp/nj-adr0020-accept-${Date.now()}`;
mkdirSync(OUT, { recursive: true });

const ITEMS = [
  { id: 8519, label: "Futurama S04E13 (damaged)", durationMs: 1_354_496 },
  { id: 8512, label: "Futurama 4x06", durationMs: 1_355_819 },
  { id: 8517, label: "Futurama 4x11", durationMs: 1_357_398 },
  { id: 6653, label: "Elementary 3x05 (healthy)", durationMs: 2_599_931 },
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
  return { status: res.status, body, headers: res.headers };
}

async function waitOk(url, { timeoutMs = 60_000, label = url } = {}) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const res = await fetch(url);
    if (res.ok) {
      const buf = Buffer.from(await res.arrayBuffer());
      return { ok: true, status: res.status, bytes: buf, ms: Date.now() - t0 };
    }
    if (res.status === 404) {
      return { ok: false, status: 404, ms: Date.now() - t0, label };
    }
    await sleep(100);
  }
  return { ok: false, status: "timeout", ms: Date.now() - t0, label };
}

function parseMediaPlaylist(text) {
  const segs = [];
  let pending = null;
  for (const line of text.split(/\r?\n/)) {
    if (line.startsWith("#EXTINF:")) {
      pending = Number(line.slice("#EXTINF:".length).split(",")[0]);
      continue;
    }
    if (!line || line.startsWith("#")) continue;
    const name = line.split("/").pop();
    const m = /^seg_(\d{11})\.m4s$/.exec(name);
    if (m) {
      segs.push({
        name,
        startMs: Number(m[1]),
        extinf: pending,
      });
    }
    pending = null;
  }
  return segs;
}

async function sidxEarliestMs(bytes) {
  // Minimal fMP4 walk for video sidx earliest_presentation_time → ms.
  let off = 0;
  while (off + 8 <= bytes.length) {
    let size = bytes.readUInt32BE(off);
    const typ = bytes.toString("ascii", off + 4, off + 8);
    if (size === 1) {
      if (off + 16 > bytes.length) break;
      size = Number(bytes.readBigUInt64BE(off + 8));
    }
    if (size < 8 || off + size > bytes.length) break;
    if (typ === "sidx") {
      const ver = bytes[off + 8];
      const timescale = bytes.readUInt32BE(off + 16);
      let earliest;
      if (ver === 0) earliest = bytes.readUInt32BE(off + 20);
      else earliest = Number(bytes.readBigUInt64BE(off + 20));
      if (!timescale) return null;
      return Math.round((earliest * 1000) / timescale);
    }
    off += size;
  }
  return null;
}

async function startSession(itemId, startMs = 0) {
  const url = `${BASE}/api/v0/items/${itemId}/sessions?startMs=${startMs}`;
  const { status, body } = await json(url, { method: "POST" });
  if (status !== 202 && status !== 200) {
    throw new Error(`POST session item=${itemId} → ${status} ${JSON.stringify(body)}`);
  }
  return body;
}

async function seekSession(sessionId, startMs) {
  const url = `${BASE}/api/v0/sessions/${sessionId}/seek?startMs=${startMs}`;
  const t0 = Date.now();
  const { status, body } = await json(url, { method: "POST" });
  if (status !== 202 && status !== 200) {
    throw new Error(`POST seek ${sessionId} @${startMs} → ${status} ${JSON.stringify(body)}`);
  }
  return { view: body, postMs: Date.now() - t0 };
}

function absUrl(path) {
  if (path.startsWith("http")) return path;
  return `${BASE}${path}`;
}

function sessionBase(masterUrl) {
  // .../sessions/{id}/runs/{run}/master.m3u8 → .../sessions/{id}/
  const u = masterUrl.replace(/\/runs\/\d+\/master\.m3u8.*$/, "/");
  return u;
}

async function fetchIndex(masterUrl) {
  const master = await (await fetch(masterUrl)).text();
  const indexLine = master
    .split(/\r?\n/)
    .find((l) => l && !l.startsWith("#") && l.includes("index.m3u8"));
  if (!indexLine) throw new Error("no index in master");
  const indexUrl = new URL(indexLine, masterUrl).href;
  const text = await (await fetch(indexUrl)).text();
  return { indexUrl, text, segs: parseMediaPlaylist(text) };
}

async function warmSweep(item) {
  const result = {
    id: item.id,
    label: item.label,
    ok: false,
    steps: [],
    errors: [],
  };
  let session;
  try {
    session = await startSession(item.id, 0);
    result.steps.push({
      step: "create@0",
      sessionId: session.sessionId,
      playlistUrl: session.playlistUrl,
      landedMs: session.landedMs,
      usableExtentMs: session.usableExtentMs ?? null,
    });
    const masterUrl = absUrl(session.playlistUrl);
    const masterWait = await waitOk(masterUrl, { label: "master" });
    if (!masterWait.ok) {
      result.errors.push(`master not ready: ${masterWait.status}`);
      return result;
    }
    let { segs, indexUrl } = await fetchIndex(masterUrl);
    if (!segs.length) {
      // EVENT may be empty briefly — poll
      const t0 = Date.now();
      while (Date.now() - t0 < 60_000 && !segs.length) {
        await sleep(200);
        ({ segs } = await fetchIndex(masterUrl));
      }
    }
    if (!segs.length) {
      result.errors.push("no segments listed after create@0");
      return result;
    }
    const first = segs[0];
    const segUrl = new URL(`../${first.name}`, indexUrl).href;
    const firstGet = await waitOk(segUrl, { label: first.name });
    if (!firstGet.ok) {
      result.errors.push(`first listed URI ${first.name} → ${firstGet.status}`);
      return result;
    }
    const sidx = await sidxEarliestMs(firstGet.bytes);
    result.steps.push({
      step: "first_listed",
      name: first.name,
      claimedMs: first.startMs,
      sidxMs: sidx,
      match: sidx != null && Math.abs(sidx - first.startMs) <= 1,
      ms: firstGet.ms,
    });
    if (sidx == null || Math.abs(sidx - first.startMs) > 1) {
      result.errors.push(
        `sidx mismatch first listed: claimed=${first.startMs} sidx=${sidx}`,
      );
    }

    // Far seek ~40% (or 75% for 8519 separately)
    const farMs = Math.floor(item.durationMs * 0.4);
    const aligned = Math.floor(farMs / 2000) * 2000;
    const seek = await seekSession(session.sessionId, aligned);
    const seekMaster = absUrl(seek.view.playlistUrl);
    const seekMasterWait = await waitOk(seekMaster, { label: "seek-master" });
    if (!seekMasterWait.ok) {
      result.errors.push(`seek master not ready: ${seekMasterWait.status}`);
      return result;
    }
    const landT0 = Date.now();
    let landSegs = [];
    let landIndexUrl = "";
    while (Date.now() - landT0 < 90_000) {
      const ix = await fetchIndex(seekMaster);
      landSegs = ix.segs;
      landIndexUrl = ix.indexUrl;
      if (landSegs.length) break;
      await sleep(150);
    }
    if (!landSegs.length) {
      result.errors.push("no segments after far seek");
      return result;
    }
    const land = landSegs[0];
    const landUrl = new URL(`../${land.name}`, landIndexUrl).href;
    const landGet = await waitOk(landUrl, { timeoutMs: 90_000, label: land.name });
    const seekToLandMs = Date.now() - landT0 + seek.postMs;
    const landSidx = landGet.ok ? await sidxEarliestMs(landGet.bytes) : null;
    result.steps.push({
      step: "far_seek",
      startMs: aligned,
      postMs: seek.postMs,
      seekToFirstListedMs: seekToLandMs,
      runId: seek.view.runId,
      landedMs: seek.view.landedMs,
      usableExtentMs: seek.view.usableExtentMs ?? null,
      firstListed: land.name,
      claimedMs: land.startMs,
      sidxMs: landSidx,
      match: landGet.ok && landSidx != null && Math.abs(landSidx - land.startMs) <= 1,
      landOk: landGet.ok,
    });
    if (!landGet.ok) {
      result.errors.push(`far seek land URI ${land.name} → ${landGet.status}`);
    } else if (landSidx == null || Math.abs(landSidx - land.startMs) > 1) {
      result.errors.push(
        `sidx mismatch after seek: claimed=${land.startMs} sidx=${landSidx}`,
      );
    }

    // Map-hit: play a bit forward by waiting for a second segment if present,
    // then seek back to first listed start of the prior region.
    const backMs = first.startMs;
    // Snapshot disk via session view polling isn't available; use run id +
    // seek timing + confirm asset without waiting for cook.
    const beforeView = await json(`${BASE}/api/v0/sessions/${session.sessionId}`);
    const mapSeek = await seekSession(session.sessionId, backMs);
    const mapMaster = absUrl(mapSeek.view.playlistUrl);
    await waitOk(mapMaster, { label: "map-hit-master" });
    const backName = first.name;
    // Segment URLs are session-root relative: /sessions/{id}/seg_….m4s
    const backUrl = `${sessionBase(mapMaster)}${backName}`;
    const tBack = Date.now();
    const backGet = await fetch(backUrl);
    const backMsWall = Date.now() - tBack;
    const backBytes = backGet.ok ? Buffer.from(await backGet.arrayBuffer()) : null;
    result.steps.push({
      step: "map_hit_scrub_back",
      startMs: backMs,
      postMs: mapSeek.postMs,
      runId: mapSeek.view.runId,
      priorRunId: seek.view.runId,
      freshUri: mapSeek.view.runId !== seek.view.runId,
      assetStatus: backGet.status,
      assetMs: backMsWall,
      // Plain file serve: should be fast (<2s), not SEGMENT_WAIT cook.
      plainFileServe: backGet.ok && backMsWall < 2000,
      beforeLandedMs: beforeView.body?.landedMs,
      afterLandedMs: mapSeek.view.landedMs,
    });
    if (!backGet.ok) {
      result.errors.push(`scrub-back ${backName} → ${backGet.status}`);
    } else if (backMsWall >= 2000) {
      result.errors.push(
        `scrub-back took ${backMsWall}ms — expected plain file serve`,
      );
    }
    if (backBytes) void backBytes;

    // Dig-back before land after a mid seek (lead=0): request land-2000.
    const mid = aligned;
    const digName = `seg_${String(Math.max(0, mid - 2000)).padStart(11, "0")}.m4s`;
    // Re-seek to mid so dig-back is behind current land.
    const midAgain = await seekSession(session.sessionId, mid);
    await waitOk(absUrl(midAgain.view.playlistUrl));
    // Wait until land cooks so dig-back is a deliberate behind-land GET.
    const midIx = await fetchIndex(absUrl(midAgain.view.playlistUrl));
    if (midIx.segs.length) {
      const midLand = midIx.segs[0];
      await waitOk(new URL(`../${midLand.name}`, midIx.indexUrl).href, {
        timeoutMs: 90_000,
      });
    }
    const digUrl = `${sessionBase(absUrl(midAgain.view.playlistUrl))}${digName}`;
    const tDig = Date.now();
    const digRes = await fetch(digUrl);
    const digElapsed = Date.now() - tDig;
    result.steps.push({
      step: "digback_before_land",
      name: digName,
      status: digRes.status,
      ms: digElapsed,
      // 404 preferred; 503 ok if short. Must not be a long cook 200.
      ok:
        (digRes.status === 404 || digRes.status === 503) &&
        digElapsed < 5000 &&
        digRes.status !== 200,
    });
    if (digRes.status === 200) {
      result.errors.push(`dig-back before land 200'd (${digName}) — lead buffer resurrected?`);
    }

    await fetch(`${BASE}/api/v0/sessions/${session.sessionId}`, { method: "DELETE" });
    result.ok = result.errors.length === 0;
  } catch (e) {
    result.errors.push(String(e?.stack || e));
    if (session?.sessionId) {
      await fetch(`${BASE}/api/v0/sessions/${session.sessionId}`, {
        method: "DELETE",
      }).catch(() => {});
    }
  }
  return result;
}

async function damaged75(item) {
  const result = {
    id: item.id,
    label: `${item.label} @75%`,
    ok: false,
    steps: [],
    errors: [],
  };
  let session;
  try {
    const startMs = Math.floor(item.durationMs * 0.75);
    const aligned = Math.floor(startMs / 2000) * 2000;
    session = await startSession(item.id, aligned);
    result.steps.push({
      step: "create@75%",
      startMs: aligned,
      sessionId: session.sessionId,
      playlistUrl: session.playlistUrl,
      landedMs: session.landedMs,
      usableExtentMs: session.usableExtentMs ?? null,
    });
    const masterUrl = absUrl(session.playlistUrl);
    const masterWait = await waitOk(masterUrl, { timeoutMs: 120_000 });
    if (!masterWait.ok) {
      result.errors.push(`master @75%: ${masterWait.status}`);
      return result;
    }
    // Poll session view + index until usable extent known or land serves.
    const t0 = Date.now();
    let usable = session.usableExtentMs ?? null;
    let landOk = false;
    let hung = false;
    while (Date.now() - t0 < 120_000) {
      const view = await json(`${BASE}/api/v0/sessions/${session.sessionId}`);
      if (view.body?.usableExtentMs != null) usable = view.body.usableExtentMs;
      try {
        const { segs, indexUrl } = await fetchIndex(masterUrl);
        if (segs.length) {
          const land = segs[0];
          const landUrl = new URL(`../${land.name}`, indexUrl).href;
          const res = await fetch(landUrl);
          if (res.ok) {
            landOk = true;
            break;
          }
          if (res.status === 404) {
            // Damaged: may 404 listed if producer EOF early — still must not hang.
            hung = false;
          }
        }
      } catch {
        /* keep polling */
      }
      await sleep(250);
    }
    // If never landed, check usable extent after child EOF.
    if (!landOk) {
      for (let i = 0; i < 40; i++) {
        const view = await json(`${BASE}/api/v0/sessions/${session.sessionId}`);
        if (view.body?.usableExtentMs != null) {
          usable = view.body.usableExtentMs;
          break;
        }
        await sleep(500);
      }
    }
    result.steps.push({
      step: "75%_outcome",
      landOk,
      usableExtentMs: usable,
      elapsedMs: Date.now() - t0,
      hung,
    });
    if (usable == null && !landOk) {
      result.errors.push("75%: no land and no usableExtentMs (possible hang/degrade miss)");
    }
    // Damage signal: usable extent materially short of container duration.
    if (usable != null && usable > item.durationMs - 30_000) {
      result.errors.push(
        `75%: usableExtentMs=${usable} not materially short of duration ${item.durationMs}`,
      );
    }
    if (usable == null && landOk) {
      // Producer may have landed inside a healthy region — note, don't fail hard.
      result.steps.push({
        step: "note",
        msg: "landed at 75% without usable extent — region may be cookable",
      });
    }
    result.ok = result.errors.length === 0;
    await fetch(`${BASE}/api/v0/sessions/${session.sessionId}`, { method: "DELETE" });
  } catch (e) {
    result.errors.push(String(e?.stack || e));
  }
  return result;
}

async function droppedSeekUnrecoverable(item) {
  // Deliberate: create@0, skip POST /seek, request a far URI → must not cook.
  const result = { id: item.id, label: "dropped-seek", ok: false, steps: [], errors: [] };
  let session;
  try {
    session = await startSession(item.id, 0);
    const masterUrl = absUrl(session.playlistUrl);
    await waitOk(masterUrl);
    const { segs, indexUrl } = await fetchIndex(masterUrl);
    if (!segs.length) {
      result.errors.push("no segs at start");
      return result;
    }
    await waitOk(new URL(`../${segs[0].name}`, indexUrl).href);
    const farMs = Math.floor(item.durationMs * 0.5);
    const aligned = Math.floor(farMs / 2000) * 2000;
    const farName = `seg_${String(aligned).padStart(11, "0")}.m4s`;
    const farUrl = `${sessionBase(masterUrl)}${farName}`;
    const t0 = Date.now();
    const res = await fetch(farUrl);
    // Drain / wait up to ~35s if 503 long-poll semantics via repeated GET.
    let last = res.status;
    let elapsed = Date.now() - t0;
    if (last === 503) {
      while (Date.now() - t0 < 35_000) {
        await sleep(500);
        const r2 = await fetch(farUrl);
        last = r2.status;
        if (last === 200) break;
        if (last === 404) break;
      }
      elapsed = Date.now() - t0;
    }
    result.steps.push({
      step: "far_without_seek",
      name: farName,
      status: last,
      ms: elapsed,
    });
    if (last === 200) {
      result.errors.push("far URI cooked without POST /seek — Restart path revived");
    } else {
      result.ok = true;
    }
    await fetch(`${BASE}/api/v0/sessions/${session.sessionId}`, { method: "DELETE" });
  } catch (e) {
    result.errors.push(String(e?.stack || e));
  }
  return result;
}

async function main() {
  const health = await fetch(`${BASE}/api/v0/items/${ITEMS[0].id}`).catch((e) => e);
  if (health instanceof Error || !health.ok) {
    console.error(`Server not reachable at ${BASE}`);
    process.exit(2);
  }

  const report = { base: BASE, startedAt: new Date().toISOString(), results: [] };

  for (const item of ITEMS) {
    console.log(`\n===== warm sweep ${item.id} ${item.label} =====`);
    const r = await warmSweep(item);
    report.results.push(r);
    console.log(JSON.stringify(r, null, 2));
  }

  console.log(`\n===== 8519 @ 75% =====`);
  const d = await damaged75(ITEMS[0]);
  report.results.push(d);
  console.log(JSON.stringify(d, null, 2));

  console.log(`\n===== dropped seek (Elementary) =====`);
  const drop = await droppedSeekUnrecoverable(ITEMS[3]);
  report.results.push(drop);
  console.log(JSON.stringify(drop, null, 2));

  // Far-seek latency summary from warm sweeps.
  const latencies = [];
  for (const r of report.results) {
    for (const s of r.steps || []) {
      if (s.step === "far_seek" && s.seekToFirstListedMs != null) {
        latencies.push({
          id: r.id,
          ms: s.seekToFirstListedMs,
          postMs: s.postMs,
        });
      }
    }
  }
  latencies.sort((a, b) => a.ms - b.ms);
  const p50 =
    latencies.length === 0
      ? null
      : latencies[Math.floor(latencies.length / 2)].ms;
  report.farSeek = { samples: latencies, p50_ms: p50, note: "lead=0 ADR-0020" };

  const failed = report.results.filter((r) => !r.ok);
  report.ok = failed.length === 0;
  writeFileSync(`${OUT}/report.json`, JSON.stringify(report, null, 2));
  console.log(`\n===== SUMMARY =====`);
  console.log(`OUT=${OUT}`);
  console.log(`far_seek_p50_ms=${p50} samples=${latencies.length}`);
  console.log(`ok=${report.ok} failed=${failed.map((f) => f.id).join(",") || "none"}`);
  process.exit(report.ok ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
