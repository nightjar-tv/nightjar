#!/usr/bin/env python3
"""Gate 2 concurrency ceiling harness (Unraid now; N150 later).

Raises concurrent 1080p HLS sessions until production drops below realtime.
No Nightjar binary change — cap via NIGHTJAR_HLS_MAX_SESSIONS on the process
under test; encoder class via whether /dev/dri is present (QSV/VAAPI vs
software-only).

Realtime = over a SAMPLE_S window after warm-up, the sum of new #EXTINF
durations is >= REALTIME_RATIO * wall seconds. Master playlists may point at
an absolute `/api/...` media URI — resolve against BASE, not the master path.

Usage:
  # Against an already-running Nightjar (prefer QSV when /dev/dri mounted):
  BASE=http://127.0.0.1:18096 python3 scripts/gate2_concurrency_ceiling.py

  # Software-only instance (start Nightjar without --device=/dev/dri first):
  BASE=http://127.0.0.1:18098 LABEL=libx264 \\
    python3 scripts/gate2_concurrency_ceiling.py

  # Remux cost alongside N transcodes:
  ALSO_REMUX=1 BASE=http://127.0.0.1:18096 python3 scripts/gate2_concurrency_ceiling.py

Env:
  BASE, LABEL (default auto from /api/v0/system/transcode preferred encoder),
  MAX_N (default 12), MIN_N (default 1), SAMPLE_S (default 20), WARMUP_S (default 8),
  REALTIME_RATIO (default 0.90), OUT_DIR, ITEM_IDS (comma), ALSO_REMUX (0/1).
"""
from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path


def http_json(url: str, method: str = "GET", timeout: float = 30.0):
    req = urllib.request.Request(url, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = resp.read()
        if not body:
            return None
        return json.loads(body.decode())


def http_code(url: str, method: str = "GET", timeout: float = 30.0) -> int:
    req = urllib.request.Request(url, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return 0


def list_items(base: str, method: str) -> list[int]:
    libs = http_json(f"{base}/api/v0/libraries")["libraries"]
    out: list[int] = []
    for lib in libs:
        items = http_json(
            f"{base}/api/v0/libraries/{lib['id']}/items?limit=200"
        )["items"]
        for it in items:
            if it.get("playbackMethod") == method:
                out.append(int(it["id"]))
    return out


def start_session(base: str, item_id: int, start_ms: int = 0) -> dict | None:
    url = f"{base}/api/v0/items/{item_id}/sessions?startMs={start_ms}"
    try:
        return http_json(url, method="POST")
    except Exception as e:
        return {"error": str(e)}


def delete_session(base: str, sid: str) -> None:
    http_code(f"{base}/api/v0/sessions/{sid}", method="DELETE")


def resolve_url(base: str, playlist_url: str, ref: str) -> str:
    if ref.startswith("http://") or ref.startswith("https://"):
        return ref
    if ref.startswith("/"):
        # Absolute path on the Nightjar host (ADR-0020 master → index).
        return f"{base}{ref}"
    root = playlist_url if playlist_url.startswith("http") else f"{base}{playlist_url}"
    root = root.rsplit("/", 1)[0]
    return f"{root}/{ref}"


def media_playlist_text(base: str, playlist_url: str) -> str:
    url = playlist_url if playlist_url.startswith("http") else f"{base}{playlist_url}"
    try:
        with urllib.request.urlopen(url, timeout=15) as resp:
            text = resp.read().decode(errors="replace")
    except Exception:
        return ""
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    if not any(ln.startswith("#EXT-X-STREAM-INF") for ln in lines):
        return text
    media = None
    for i, ln in enumerate(lines):
        if ln.startswith("#EXT-X-STREAM-INF") and i + 1 < len(lines):
            media = lines[i + 1]
            break
    if not media:
        return ""
    media_url = resolve_url(base, url, media)
    try:
        with urllib.request.urlopen(media_url, timeout=15) as resp:
            return resp.read().decode(errors="replace")
    except Exception:
        return ""


def playlist_media_seconds(base: str, playlist_url: str) -> float:
    """Sum of #EXTINF durations in the media playlist (0 if unreadable)."""
    text = media_playlist_text(base, playlist_url)
    total = 0.0
    for ln in text.splitlines():
        if ln.startswith("#EXTINF:"):
            raw = ln.split(":", 1)[1].split(",", 1)[0].strip()
            try:
                total += float(raw)
            except ValueError:
                pass
    return total


def measure_n(
    base: str,
    item_ids: list[int],
    n: int,
    warmup_s: float,
    sample_s: float,
) -> dict:
    """Open n sessions on rotating items; measure segment growth rate."""
    sessions: list[dict] = []
    errors: list[str] = []
    for i in range(n):
        item = item_ids[i % len(item_ids)]
        # Stagger startMs slightly so item-keyed share cannot collapse all to one.
        body = start_session(base, item, start_ms=i * 120_000)
        if not body or not body.get("sessionId"):
            errors.append(f"create_failed i={i} body={body}")
            break
        sessions.append(body)
        time.sleep(0.3)

    if len(sessions) < n:
        for s in sessions:
            delete_session(base, s["sessionId"])
        return {
            "n": n,
            "opened": len(sessions),
            "ok": False,
            "reason": "could_not_open_all",
            "errors": errors,
        }

    time.sleep(warmup_s)
    before = [playlist_media_seconds(base, s.get("playlistUrl") or "") for s in sessions]
    t0 = time.time()
    time.sleep(sample_s)
    elapsed = time.time() - t0
    after = [playlist_media_seconds(base, s.get("playlistUrl") or "") for s in sessions]
    deltas = [max(0.0, a - b) for a, b in zip(after, before)]

    for s in sessions:
        delete_session(base, s["sessionId"])
    time.sleep(2)

    # Media seconds produced / wall seconds. 1.0 == keeping up with realtime.
    ratios = [(d / elapsed) if elapsed > 0 else 0.0 for d in deltas]
    min_ratio = min(ratios) if ratios else 0.0
    return {
        "n": n,
        "opened": n,
        "ok": True,
        "elapsedS": round(elapsed, 3),
        "mediaSecondsDeltas": [round(d, 3) for d in deltas],
        "realtimeRatios": [round(r, 3) for r in ratios],
        "minRealtimeRatio": round(min_ratio, 3),
        "errors": errors,
    }


def main() -> int:
    base = os.environ.get("BASE", "http://127.0.0.1:8096").rstrip("/")
    out_dir = Path(os.environ.get("OUT_DIR", "notes/hw"))
    out_dir.mkdir(parents=True, exist_ok=True)
    max_n = int(os.environ.get("MAX_N", "12"))
    min_n = int(os.environ.get("MIN_N", "1"))
    warmup_s = float(os.environ.get("WARMUP_S", "15"))
    sample_s = float(os.environ.get("SAMPLE_S", "40"))
    ratio_need = float(os.environ.get("REALTIME_RATIO", "0.90"))
    also_remux = os.environ.get("ALSO_REMUX", "0") in ("1", "true", "yes")
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())

    if http_code(f"{base}/api/health") != 200:
        print(f"FAIL: {base}/api/health", file=sys.stderr)
        return 1

    caps = http_json(f"{base}/api/v0/system/transcode")
    label = os.environ.get("LABEL") or caps.get("preferredH264Encoder", "unknown")

    if os.environ.get("ITEM_IDS"):
        items = [int(x) for x in os.environ["ITEM_IDS"].split(",") if x.strip()]
    else:
        items = list_items(base, "transcode")
    if not items:
        print("FAIL: no transcode items", file=sys.stderr)
        return 1

    remux_items = list_items(base, "remux") if also_remux else []

    results = []
    ceiling = None
    for n in range(min_n, max_n + 1):
        remux_sids = []
        if also_remux and remux_items:
            body = start_session(base, remux_items[0], start_ms=0)
            if body and body.get("sessionId"):
                remux_sids.append(body["sessionId"])

        row = measure_n(base, items, n, warmup_s, sample_s)
        row["label"] = label
        row["remuxAlongside"] = len(remux_sids)
        for sid in remux_sids:
            delete_session(base, sid)

        results.append(row)
        print(json.dumps(row), flush=True)

        if not row.get("ok"):
            ceiling = {"nFailOpen": n, "lastOk": n - 1, "limit": "admission_or_error"}
            break
        if row["minRealtimeRatio"] < ratio_need:
            ceiling = {
                "nFailRealtime": n,
                "lastOk": n - 1,
                "limit": "below_realtime",
                "minRealtimeRatio": row["minRealtimeRatio"],
            }
            break
    else:
        ceiling = {"lastOk": max_n, "limit": "not_reached_within_MAX_N"}

    summary = {
        "stamp": stamp,
        "base": base,
        "label": label,
        "preferredH264Encoder": caps.get("preferredH264Encoder"),
        "encoders": caps.get("encoders"),
        "ratioNeed": ratio_need,
        "warmupS": warmup_s,
        "sampleS": sample_s,
        "itemIdsUsed": items[: max_n + 1],
        "ceiling": ceiling,
        "rows": results,
    }
    out_json = out_dir / f"concurrency-ceiling-{label}-{stamp}.json"
    out_json.write_text(json.dumps(summary, indent=2) + "\n")
    print(f"wrote {out_json}", flush=True)
    print(json.dumps({"ceiling": ceiling, "label": label}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
