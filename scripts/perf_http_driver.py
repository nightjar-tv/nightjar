#!/usr/bin/env python3
"""Authenticated HTTP driver and measurement harness for slice PERF-1A.

Times the real show-browse (`GET /api/v0/libraries/{id}/units`) and rail
(`GET /api/v0/profiles/{ref}/continue-watching`) routes against a running
server. It reads the whole unpaginated browse body and verifies it; it never
truncates a response to simulate a page.

Credentials come from a mode-0600 file. They are never passed on argv and never
written to evidence. Only the product endpoints are called.

Usage:
  perf_http_driver.py prepare --base-url URL --password-file PW \
      --token-out TOKEN --state-out STATE.json
  perf_http_driver.py selftest
  perf_http_driver.py run --base-url URL --token-file TOKEN --state STATE.json \
      --scale 100k --requests 100 --out RESULT.json [--server-pid PID]
  perf_http_driver.py run ... --readers 4 --endpoints units,detail,rail
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

# The self-test's positive control must be measured at least this slow, or the
# instrument is not live (Rule 4.15).
SELFTEST_SLEEP_S = 0.06
SELFTEST_SLEEP_FLOOR_MS = 40
SELFTEST_FLAG_MS = 10
# A no-op must not be flagged against a budget it cannot reach.
SELFTEST_FAST_BUDGET_MS = 1000

# The capped profile the rail is measured against. The generator mirrors the
# same cap and region when it chooses the history it seeds, so the rail's
# server-side visibility filter and the manifest's expected rail agree.
CAPPED_PROFILE_CAP = "teen"
CLASSIFICATION_REGION = "US"


def _fail(message: str) -> None:
    print(f"perf_http_driver: {message}", file=sys.stderr)
    raise SystemExit(1)


def _read_secret(path: Path) -> str:
    if not path.exists():
        _fail(f"secret file missing: {path}")
    mode = path.stat().st_mode & 0o777
    if mode & 0o077:
        _fail(f"secret file {path} is mode {mode:o}, must be 0600")
    return path.read_text().strip()


class Client:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.token = token

    def request(self, method: str, path: str, body: dict | None = None) -> tuple[int, bytes]:
        data = None
        headers = {"Authorization": f"Bearer {self.token}"}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(
            f"{self.base_url}{path}", data=data, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(req, timeout=120) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()


def _prepare(client: Client, password: str, token_out: Path, state_out: Path) -> int:
    username = "perf_harness"
    status, body = client.request(
        "POST",
        "/api/v0/auth/bootstrap",
        {
            "username": username,
            "password": password,
            "classificationRegion": CLASSIFICATION_REGION,
            "clientLabel": "perf harness",
        },
    )
    if status == 409:
        status, body = client.request(
            "POST",
            "/api/v0/auth/login",
            {"username": username, "password": password, "clientLabel": "perf harness"},
        )
    if status not in (200, 201):
        _fail(f"auth failed with status {status}")
    token = json.loads(body)["token"]
    client.token = token

    status, body = client.request("GET", "/api/v0/profiles")
    if status != 200:
        _fail(f"profiles failed with status {status}")
    default_ref = json.loads(body)["profiles"][0]["profileRef"]

    status, body = client.request(
        "POST",
        "/api/v0/profiles",
        {"name": "perf-capped", "classificationCap": CAPPED_PROFILE_CAP},
    )
    if status not in (200, 201):
        _fail(f"capped profile create failed with status {status}")
    capped_ref = json.loads(body)["profileRef"]

    status, _ = client.request(
        "POST", "/api/v0/auth/session", {"profileRef": capped_ref}
    )
    if status != 204:
        _fail(f"session narrow failed with status {status}")

    status, body = client.request("GET", "/api/v0/libraries")
    if status != 200:
        _fail(f"libraries failed with status {status}")
    libraries = json.loads(body)["libraries"]
    if not libraries:
        _fail("no library is registered")
    library_id = libraries[0]["id"]

    token_out.write_text(token)
    os.chmod(token_out, 0o600)
    state_out.write_text(
        json.dumps(
            {
                "library_id": library_id,
                "default_profile_ref": default_ref,
                "capped_profile_ref": capped_ref,
                "capped_profile_cap": CAPPED_PROFILE_CAP,
                "classification_region": CLASSIFICATION_REGION,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    print(json.dumps({"library_id": library_id, "capped_profile_ref": capped_ref}))
    return 0


def _measure_ms(callable_) -> tuple[float, object]:
    start = time.perf_counter()
    result = callable_()
    elapsed_ms = (time.perf_counter() - start) * 1000.0
    return elapsed_ms, result


def _percentile(sorted_values: list[float], fraction: float) -> float:
    if not sorted_values:
        return 0.0
    index = min(len(sorted_values) - 1, int(round(fraction * (len(sorted_values) - 1))))
    return sorted_values[index]


def latency_breach(samples_ms: list[float], budget_ms: float) -> bool:
    """The one latency gate. It must be able to say yes, or it is not a gate."""
    return _percentile(sorted(samples_ms), 0.50) > budget_ms


def _verify_units(body: bytes) -> dict:
    payload = json.loads(body)
    units = payload["units"]
    counts = payload["counts"]
    if len(units) != counts["units"]:
        raise ValueError(
            f"units length {len(units)} != counts.units {counts['units']}"
        )
    keys = [unit["seriesKey"] for unit in units]
    if len(set(keys)) != len(keys):
        raise ValueError("duplicate seriesKey in the complete response")
    if sum(unit["itemCount"] for unit in units) != counts["items"]:
        raise ValueError("unit item counts do not sum to counts.items")
    if counts["items"] <= 0:
        raise ValueError("counts.items is zero; the instrument saw no library")
    return {"units": len(units), "items": counts["items"]}


def _verify_rail(body: bytes, expected: list | None) -> dict:
    """Assert the rail's server-limited membership and order, not only non-empty.

    The server collapses one show to one entry and sorts by `lastPlayedAt` DESC
    with the series key as the tie-break (ADR-0035 item 8). The manifest carries
    the same projection, derived from the rows actually written to the DB, so a
    rail that is missing a series, invents one, or orders differently fails.
    """
    payload = json.loads(body)
    items = payload["items"]
    if not isinstance(items, list):
        raise ValueError("rail items is not a list")
    if expected is None:
        raise ValueError("manifest carries no rail_expected projection")
    if len(items) != len(expected):
        raise ValueError(
            f"rail returned {len(items)} entries, expected {len(expected)}"
        )
    seen: set[str] = set()
    previous: tuple[str, str] | None = None
    for index, (item, wanted) in enumerate(zip(items, expected)):
        series_key = item["seriesKey"]
        if series_key != wanted["seriesKey"]:
            raise ValueError(
                f"rail[{index}].seriesKey {series_key!r} != {wanted['seriesKey']!r}"
            )
        if item["itemKey"] != wanted["itemKey"]:
            raise ValueError(
                f"rail[{index}].itemKey {item['itemKey']!r} != {wanted['itemKey']!r}"
            )
        if item["lastPlayedAt"] != wanted["lastPlayedAt"]:
            raise ValueError(
                f"rail[{index}].lastPlayedAt {item['lastPlayedAt']!r} != "
                f"{wanted['lastPlayedAt']!r}"
            )
        if series_key in seen:
            raise ValueError(f"rail lists series {series_key!r} twice")
        seen.add(series_key)
        current = (item["lastPlayedAt"], series_key)
        if previous is not None and (
            previous[0] < current[0]
            or (previous[0] == current[0] and previous[1] > current[1])
        ):
            raise ValueError(
                f"rail is not sorted at index {index}: {previous} then {current}"
            )
        previous = current
    return {"entries": len(items), "series": len(seen)}


def _verify_detail(body: bytes, item_id: int, item_key: str) -> dict:
    """Assert the detail route returned the item the rail named.

    The rail entry is the discovery source, so this proves the detail contract
    agrees with the rail's `itemId` and effective `itemKey` rather than only
    that the route answered 200.
    """
    payload = json.loads(body)
    if payload.get("id") != item_id:
        raise ValueError(f"detail id {payload.get('id')!r} != requested {item_id}")
    if payload.get("itemKey") != item_key:
        raise ValueError(
            f"detail itemKey {payload.get('itemKey')!r} != rail {item_key!r}"
        )
    if not payload.get("path"):
        raise ValueError("detail carries no path")
    return {
        "id": item_id,
        "itemKey": item_key,
        "kind": payload.get("kind"),
        "has_series_key": bool(payload.get("seriesKey")),
    }


def _run_endpoint(
    client: Client,
    path: str,
    verify,
    requests: int,
    warm: int,
    readers: int = 1,
) -> dict:
    """Measure one endpoint with `readers` concurrent clients.

    `requests` is the total measured requests for the endpoint, split as evenly
    as possible across the readers. Warm-up is sequential and happens once,
    before the readers start, so the route is warm for every reader. A 200
    response counts toward the latency sample even if its body fails
    verification; the correctness count is kept separate so a wrong body cannot
    hide inside a fast p95.
    """
    if readers < 1:
        _fail(f"readers must be at least 1, got {readers}")
    for _ in range(warm):
        status, body = client.request("GET", path)
        if status != 200:
            _fail(f"warm request to {path} failed with status {status}")
        verify(body)

    lock = threading.Lock()
    durations: list[float] = []
    sizes: list[int] = []
    errors = 0
    correctness_errors = 0
    observed: dict = {}
    per_reader: list[int] = []

    shares = [
        requests // readers + (1 if index < requests % readers else 0)
        for index in range(readers)
    ]

    def worker(share: int) -> None:
        nonlocal errors, correctness_errors, observed
        local_completed = 0
        for _ in range(share):
            try:
                elapsed_ms, (status, body) = _measure_ms(
                    lambda: client.request("GET", path)
                )
            except Exception:
                with lock:
                    errors += 1
                continue
            if status != 200:
                with lock:
                    errors += 1
                continue
            try:
                seen = verify(body)
            except (ValueError, KeyError, TypeError):
                with lock:
                    correctness_errors += 1
                    durations.append(elapsed_ms)
                    sizes.append(len(body))
                continue
            with lock:
                durations.append(elapsed_ms)
                sizes.append(len(body))
                observed = seen
                local_completed += 1
        with lock:
            per_reader.append(local_completed)

    wall_start = time.perf_counter()
    threads = [threading.Thread(target=worker, args=(share,)) for share in shares]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    wall_ms = (time.perf_counter() - wall_start) * 1000.0

    ordered = sorted(durations)
    return {
        "path": path,
        "readers": readers,
        "requests": requests,
        "warm": warm,
        "completed": len(durations),
        "errors": errors,
        "correctness_errors": correctness_errors,
        "p50_ms": round(_percentile(ordered, 0.50), 3),
        "p95_ms": round(_percentile(ordered, 0.95), 3),
        "p99_ms": round(_percentile(ordered, 0.99), 3),
        "max_ms": round(max(ordered), 3) if ordered else 0.0,
        "wall_ms": round(wall_ms, 3),
        "per_reader_completed": sorted(per_reader),
        "bytes_p50": _percentile([float(s) for s in sorted(sizes)], 0.50),
        "bytes_max": max(sizes) if sizes else 0,
        "observed": observed,
    }


def _provenance(root: Path, manifest_path: Path | None, seed: int | None) -> dict:
    try:
        commit = subprocess.check_output(
            ["git", "-C", str(root), "rev-parse", "HEAD"], text=True
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        commit = "unknown"
    binary = root / "server/target/release/nightjar"
    digest = "missing"
    if binary.exists():
        hasher = hashlib.sha256()
        with binary.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                hasher.update(chunk)
        digest = hasher.hexdigest()
    manifest_sha = "missing"
    if manifest_path is not None and manifest_path.exists():
        manifest_sha = json.loads(manifest_path.read_text()).get("manifest_sha256", "missing")
    return {
        "git_commit": commit,
        "binary_sha256": digest,
        "manifest_sha256": manifest_sha,
        "seed": seed,
    }


def _server_stats(pid: int) -> dict:
    try:
        out = subprocess.check_output(
            ["ps", "-o", "%cpu=,rss=", "-p", str(pid)], text=True
        ).split()
        return {"cpu_percent": float(out[0]), "rss_kb": int(out[1])}
    except (subprocess.CalledProcessError, ValueError, IndexError):
        return {"cpu_percent": None, "rss_kb": None}


def _db_stats(db_path: Path) -> dict:
    """The database counters SQLite exposes for the disposable file.

    `page_count * page_size` is the whole-file size and `freelist_count` is the
    free pages inside it; the WAL and shared-memory sidecars are counted by
    their real file sizes. No server-side query or fetched-row counter exists
    on these routes, so those are absent rather than invented.
    """
    if not db_path.exists():
        return {"exists": False}
    stats: dict = {"exists": True, "path": str(db_path)}
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            page_count = int(conn.execute("PRAGMA page_count").fetchone()[0])
            page_size = int(conn.execute("PRAGMA page_size").fetchone()[0])
            freelist = int(conn.execute("PRAGMA freelist_count").fetchone()[0])
        finally:
            conn.close()
        stats.update(
            {
                "page_count": page_count,
                "page_size": page_size,
                "db_bytes": page_count * page_size,
                "freelist_count": freelist,
            }
        )
    except sqlite3.Error as error:
        stats["error"] = str(error)
    for suffix, key in (("-wal", "wal_bytes"), ("-shm", "shm_bytes")):
        side = Path(str(db_path) + suffix)
        stats[key] = side.stat().st_size if side.exists() else 0
    return stats


def _cmd_selftest(_: argparse.Namespace) -> int:
    slow_ms, _ = _measure_ms(lambda: time.sleep(SELFTEST_SLEEP_S))
    fast_ms, _ = _measure_ms(lambda: None)
    detected = (
        slow_ms >= SELFTEST_SLEEP_FLOOR_MS
        and latency_breach([slow_ms], SELFTEST_FLAG_MS)
    )
    not_flagged = not latency_breach([fast_ms], SELFTEST_FAST_BUDGET_MS)
    report = {
        "slow_measured_ms": round(slow_ms, 3),
        "fast_measured_ms": round(fast_ms, 3),
        "detected_deliberate_slowness": detected,
        "fast_not_flagged": not_flagged,
    }
    print(json.dumps(report, sort_keys=True))
    if not detected or not not_flagged:
        return 1
    return 0


def _cmd_prepare(args: argparse.Namespace) -> int:
    password = _read_secret(Path(args.password_file))
    client = Client(args.base_url, "")
    return _prepare(client, password, Path(args.token_out), Path(args.state_out))


def _cmd_run(args: argparse.Namespace) -> int:
    token = _read_secret(Path(args.token_file))
    client = Client(args.base_url, token)
    state = json.loads(Path(args.state).read_text())
    library_id = state["library_id"]
    capped_ref = state["capped_profile_ref"]

    manifest = json.loads(Path(args.manifest).read_text()) if args.manifest else {}
    rail_expected = manifest.get("rail_expected")
    if not rail_expected:
        _fail("manifest carries no rail_expected projection; run verify-history first")

    endpoints = [name.strip() for name in args.endpoints.split(",") if name.strip()]
    unknown = set(endpoints) - {"units", "rail", "detail"}
    if unknown:
        _fail(f"unknown endpoint(s): {', '.join(sorted(unknown))}")
    if not endpoints:
        _fail("--endpoints named no endpoint")

    units_path = f"/api/v0/libraries/{library_id}/units"
    rail_path = f"/api/v0/profiles/{capped_ref}/continue-watching"

    # The rail is the discovery source for a visible item: it is already scoped
    # to the caller's profile, so its first entry names an item the detail route
    # must also serve. One request, not part of any measured cell.
    detail_item_id: int | None = None
    detail_item_key: str | None = None
    if "detail" in endpoints:
        status, body = client.request("GET", rail_path)
        if status != 200:
            _fail(f"detail discovery through the rail failed with status {status}")
        entries = json.loads(body).get("items") or []
        if not entries:
            _fail("rail is empty; no visible item for the detail cell")
        detail_item_id = int(entries[0]["itemId"])
        detail_item_key = str(entries[0]["itemKey"])

    stats_before = _server_stats(args.server_pid) if args.server_pid else None
    measured: dict = {}
    if "units" in endpoints:
        measured["units"] = _run_endpoint(
            client, units_path, _verify_units, args.requests, args.warm, args.readers
        )
    if "rail" in endpoints:
        measured["rail"] = _run_endpoint(
            client,
            rail_path,
            lambda body: _verify_rail(body, rail_expected),
            args.requests,
            args.warm,
            args.readers,
        )
    if "detail" in endpoints:
        measured["detail"] = _run_endpoint(
            client,
            f"/api/v0/items/{detail_item_id}",
            lambda body: _verify_detail(body, detail_item_id, detail_item_key),
            args.requests,
            args.warm,
            args.readers,
        )
    stats_after = _server_stats(args.server_pid) if args.server_pid else None

    result = {
        "scale": args.scale,
        "requests_per_endpoint": args.requests,
        "readers": args.readers,
        "endpoints_requested": endpoints,
        "warm": args.warm,
        "endpoints": measured,
        "library": {
            "counts": manifest.get("counts", {}),
            "rail_expected_entries": len(rail_expected),
        },
        "server": {"before": stats_before, "after": stats_after},
        "database": _db_stats(Path(args.db_path)) if args.db_path else {},
        "detail_item": {"item_id": detail_item_id, "item_key": detail_item_key},
        "provenance": _provenance(
            Path(args.root).resolve(),
            Path(args.manifest) if args.manifest else None,
            args.seed,
        ),
    }
    output = Path(args.out).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))
    total_errors = sum(cell["errors"] for cell in measured.values())
    total_correctness = sum(cell["correctness_errors"] for cell in measured.values())
    if total_errors or total_correctness:
        return 1
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    prepare = sub.add_parser("prepare", help="bootstrap auth and narrow to a capped profile")
    prepare.add_argument("--base-url", required=True)
    prepare.add_argument("--password-file", required=True)
    prepare.add_argument("--token-out", required=True)
    prepare.add_argument("--state-out", required=True)
    prepare.set_defaults(func=_cmd_prepare)

    selftest = sub.add_parser("selftest", help="prove the timing instrument detects slowness")
    selftest.set_defaults(func=_cmd_selftest)

    run = sub.add_parser("run", help="measure the browse, detail and rail routes")
    run.add_argument("--base-url", required=True)
    run.add_argument("--token-file", required=True)
    run.add_argument("--state", required=True)
    run.add_argument("--scale", required=True)
    run.add_argument("--requests", type=int, default=100)
    run.add_argument("--warm", type=int, default=10)
    run.add_argument(
        "--readers",
        type=int,
        default=1,
        help="concurrent readers; --requests is the total across all readers",
    )
    run.add_argument(
        "--endpoints",
        default="units,rail",
        help="comma-separated subset of units,detail,rail",
    )
    run.add_argument("--server-pid", type=int)
    run.add_argument("--db-path", help="disposable SQLite file for database counters")
    run.add_argument("--root", default=".")
    run.add_argument("--manifest")
    run.add_argument("--seed", type=int)
    run.add_argument("--out", required=True)
    run.set_defaults(func=_cmd_run)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
