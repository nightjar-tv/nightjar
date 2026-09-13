#!/usr/bin/env python3
"""Deterministic disposable large-library generator (slice PERF-1A).

Builds a shows library of an exact media-item count into a fresh Nightjar data
directory, so the authenticated browse and rail routes can be timed against a
known library without touching the dogfood database. It writes rows directly to
the migrated SQLite file and hardlinks one small seed file for the media tree.

The generator is deterministic for a given (items, seed): every row it writes is
a pure function of those two inputs. The manifest hash it emits covers only
those deterministic columns. Timestamps, auth rows and profile ids are written
but deliberately excluded from the hash.

Requires: python3 (stdlib only) and a Nightjar release binary to run migrations
once (`--nightjar-bin`). No new dependency.

Usage:
  perf_large_library.py generate --items 25000 --data-dir D --media-root M \
      --seed 1 --nightjar-bin path/to/nightjar
  perf_large_library.py seed-history --data-dir D --profile-ref REF
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

# The acceptance fixes this count, so it is a constant and not a flag.
HISTORY_ROWS = 5000
EPISODES_PER_SHOW = 10
# One file per (show, episode) plus a duplicate for every DUPLICATE_EVERY-th
# item; the totals still land on the requested count because the generator
# stops the walk there.
DUPLICATE_EVERY = 20
# Show classification by index mod 100. The bands are disjoint and cover the
# three ADR-0039 identity states the browse route reports.
UNMATCHED_BAND = 8
ENTITY_ONLY_BAND = 12

# Valid labels from the shipped ladder (core/src/certification_ladder.json).
US_LABELS = ["TV-Y7", "TV-PG", "TV-14", "TV-MA"]
OTHER_REGIONS = {
    "AU": ["M", "MA 15+"],
    "DE": ["12", "16"],
    "BR": ["12", "14"],
    "GB": ["12", "15"],
}

# The cap and region the driver's `prepare` gives the capped profile. The
# history seeder mirrors the shipped ladder for exactly this pair, so every row
# it writes is one the capped rail can serve.
DEFAULT_CAP = "teen"
DEFAULT_REGION = "US"
# The shipped ladder is the one source of "at or under cap" (ADR-0037 item 4).
# Reading the product's own snapshot keeps this script from owning a second
# copy that could drift.
LADDER_PATH = (
    Path(__file__).resolve().parent.parent
    / "server"
    / "crates"
    / "core"
    / "src"
    / "certification_ladder.json"
)
TIER_ORDER = {"little_kid": 0, "big_kid": 1, "teen": 2, "adult": 3}


def _region_ladder() -> dict:
    with LADDER_PATH.open(encoding="utf-8") as handle:
        return json.load(handle)["regions"]


def _label_at_or_under_cap(regions: dict, region: str, label: str, cap: str) -> bool:
    cap_tier = TIER_ORDER[cap]
    for rung in regions.get(region, []):
        if rung["label"] == label:
            return TIER_ORDER[rung["tier"]] <= cap_tier
    return False


def _fail(message: str) -> None:
    print(f"perf_large_library: {message}", file=sys.stderr)
    raise SystemExit(1)


def _sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def _has_schema(db_path: Path) -> bool:
    if not db_path.exists():
        return False
    try:
        conn = sqlite3.connect(db_path)
        try:
            names = {
                row[0]
                for row in conn.execute(
                    "SELECT name FROM sqlite_master WHERE type = 'table'"
                )
            }
            if not {"libraries", "media_items", "metadata_canonical", "watch_state"} <= names:
                return False
            columns = {
                row[1]
                for row in conn.execute("PRAGMA table_info(metadata_canonical)")
            }
            return "certifications_projection_version" in columns
        finally:
            conn.close()
    except sqlite3.Error:
        return False


def ensure_migrated(data_dir: Path, nightjar_bin: Path) -> None:
    """Run the shipped server once so its own migrator creates the schema.

    Applying the migration SQL from this script would skip the code steps the
    migrator performs (the foreign-key-off table rebuilds), so the product's
    migrator is the only writer of schema here.
    """
    db_path = data_dir / "nightjar.db"
    if _has_schema(db_path):
        return
    if not nightjar_bin.exists():
        _fail(f"no migrated database at {db_path} and no binary at {nightjar_bin}")
    data_dir.mkdir(parents=True, exist_ok=True)
    port = _free_port()
    env = dict(os.environ)
    env["NIGHTJAR_DATA_DIR"] = str(data_dir)
    env["NIGHTJAR_PORT"] = str(port)
    log = (data_dir / "migrate.log").open("w")
    proc = subprocess.Popen(
        [str(nightjar_bin)], stdout=log, stderr=subprocess.STDOUT, env=env
    )
    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                _fail("server exited before the schema existed; see migrate.log")
            try:
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{port}/api/health", timeout=0.2
                ) as response:
                    if response.status == 200:
                        break
            except (urllib.error.URLError, OSError):
                time.sleep(0.05)
        else:
            _fail("server never became healthy during migration")
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        log.close()
    if not _has_schema(db_path):
        _fail("server started but the expected schema is missing")


def _show_folder(index: int) -> str:
    return f"Show {index:05d}"


def _classify(show_index: int) -> str:
    band = show_index % 100
    if band < UNMATCHED_BAND:
        return "unmatched"
    if band < ENTITY_ONLY_BAND:
        return "entity_only"
    return "bound"


def _certifications(show_index: int) -> str:
    """A deterministic mix of regional labels, or '{}' for no label."""
    kind = show_index % 7
    if kind == 0:
        return "{}"
    if kind == 1:
        return json.dumps({"US": US_LABELS[show_index % len(US_LABELS)]}, separators=(",", ":"))
    if kind == 2:
        return json.dumps({"US": "TV-MA"}, separators=(",", ":"))
    if kind == 3:
        region = ["AU", "DE", "BR", "GB"][show_index % 4]
        label = OTHER_REGIONS[region][show_index % len(OTHER_REGIONS[region])]
        return json.dumps({region: label}, separators=(",", ":"))
    if kind == 4:
        return json.dumps({"US": "TV-PG", "GB": "PG"}, separators=(",", ":"))
    if kind == 5:
        return json.dumps({"US": "TV-14", "DE": "16"}, separators=(",", ":"))
    return json.dumps({"US": US_LABELS[show_index % len(US_LABELS)]}, separators=(",", ":"))


def _episode_path(show_index: int, episode: int, duplicate: bool) -> str:
    suffix = " - copy" if duplicate else ""
    return (
        f"{_show_folder(show_index)}/Season 01/"
        f"S01E{episode:02d}{suffix}.mkv"
    )


def build_plan(items: int) -> dict:
    """The full deterministic row plan. No I/O, no clock, no randomness."""
    shows: list[dict] = []
    media_items: list[dict] = []
    links: list[dict] = []
    canonical_tv: list[dict] = []
    canonical_episode: list[dict] = []
    series_rows: list[dict] = []

    show_index = 0
    episode_id = 1
    while len(media_items) < items:
        show_class = _classify(show_index)
        folder = _show_folder(show_index)
        show_id = 100000 + show_index
        # Entity-only and unmatched shows have no `series` row; bound shows do.
        if show_class == "bound":
            series_rows.append({"relpath": folder, "tmdb_show_id": show_id})
        if show_class != "unmatched":
            canonical_tv.append(
                {
                    "provider_id": str(show_id),
                    "title": f"Perf Show {show_index:05d}",
                    "year": 1990 + (show_index % 35),
                    "certifications_json": _certifications(show_index),
                }
            )
        for episode in range(1, EPISODES_PER_SHOW + 1):
            if len(media_items) >= items:
                break
            duplicate = False
            path = _episode_path(show_index, episode, duplicate)
            media_items.append(
                {
                    "path": path,
                    "title": f"Episode {episode}",
                    "season": 1,
                    "episode": episode,
                    "duplicate": duplicate,
                }
            )
            if show_class != "unmatched":
                links.append(
                    {
                        "path": path,
                        "item_key": f"tmdb:episode:{episode_id}",
                    }
                )
                canonical_episode.append(
                    {
                        "provider_id": str(episode_id),
                        "title": f"Episode {episode}",
                        "season": 1,
                        "episode": episode,
                        "tmdb_show": show_id,
                    }
                )
                # Every DUPLICATE_EVERY-th linked episode gets a second file
                # that shares the episode's canonical item key.
                if episode_id % DUPLICATE_EVERY == 0 and len(media_items) < items:
                    copy_path = _episode_path(show_index, episode, True)
                    media_items.append(
                        {"path": copy_path, "title": f"Episode {episode}",
                         "season": 1, "episode": episode, "duplicate": True}
                    )
                    links.append({"path": copy_path,
                                  "item_key": f"tmdb:episode:{episode_id}"})
            episode_id += 1
        shows.append(
            {
                "index": show_index,
                "folder": folder,
                "class": show_class,
                "tmdb_show_id": show_id,
            }
        )
        show_index += 1
    return {
        "shows": shows,
        "media_items": media_items,
        "links": links,
        "canonical_tv": canonical_tv,
        "canonical_episode": canonical_episode,
        "series_rows": series_rows,
    }


def _seed_files(plan: dict, media_root: Path, seed_file: Path) -> None:
    if not seed_file.exists():
        _fail(f"seed media file missing: {seed_file}")
    for item in plan["media_items"]:
        destination = media_root / item["path"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        try:
            os.link(seed_file, destination)
        except FileExistsError:
            continue


def _seed_rows(conn: sqlite3.Connection, plan: dict, media_root: Path) -> None:
    conn.execute("PRAGMA foreign_keys = ON")
    conn.execute(
        "INSERT INTO libraries (id, name, path, kind, reachable, paths_unresolved, "
        "skipped_outside_root) VALUES (1, 'perf-shows', ?, 'shows', 1, 0, 0)",
        (str(media_root),),
    )
    for show in plan["series_rows"]:
        conn.execute(
            "INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, ?, ?)",
            (show["relpath"], show["tmdb_show_id"]),
        )
        conn.execute(
            "INSERT INTO series_entity_bindings "
            "(library_id, relpath, tmdb_show_id, is_primary) VALUES (1, ?, ?, 1)",
            (show["relpath"], show["tmdb_show_id"]),
        )
    projected_at = "1970-01-01T00:00:00.000Z"
    for row in plan["canonical_tv"]:
        conn.execute(
            "INSERT INTO metadata_canonical "
            "(provider, entity_kind, provider_id, title, year, ids_json, projected_at, "
            "certifications_json, certifications_projection_version) "
            "VALUES ('tmdb', 'tv', ?, ?, ?, '{}', ?, ?, 1)",
            (
                row["provider_id"],
                row["title"],
                row["year"],
                projected_at,
                row["certifications_json"],
            ),
        )
    for row in plan["canonical_episode"]:
        conn.execute(
            "INSERT INTO metadata_canonical "
            "(provider, entity_kind, provider_id, title, season, episode, tmdb_show, "
            "ids_json, projected_at) VALUES ('tmdb', 'episode', ?, ?, ?, ?, ?, '{}', ?)",
            (
                row["provider_id"],
                row["title"],
                row["season"],
                row["episode"],
                row["tmdb_show"],
                projected_at,
            ),
        )
    item_ids: dict[str, int] = {}
    for item in plan["media_items"]:
        cursor = conn.execute(
            "INSERT INTO media_items "
            "(library_id, path, mtime_ms, size_bytes, title, kind, season, episode, "
            "duration_ms, probe_status, metadata_status, subtitle_status, map_status) "
            "VALUES (1, ?, 1700000000000, 1024, ?, 'episode', ?, ?, 1800000, "
            "'probed', 'ready', 'pending', 'pending')",
            (item["path"], item["title"], item["season"], item["episode"]),
        )
        item_ids[item["path"]] = int(cursor.lastrowid)
    for link in plan["links"]:
        conn.execute(
            "INSERT INTO media_item_links (media_item_id, item_key) VALUES (?, ?)",
            (item_ids[link["path"]], link["item_key"]),
        )


def _profile_id(conn: sqlite3.Connection, profile_ref: str) -> int:
    row = conn.execute(
        "SELECT id FROM profiles WHERE profile_ref = ?", (profile_ref,)
    ).fetchone()
    if row is None:
        _fail(f"no profile with ref {profile_ref!r}")
    return int(row[0])


def eligible_episode_keys(
    conn: sqlite3.Connection, cap: str, region: str
) -> list[str]:
    """Episode item keys the capped profile can actually see, from the DB.

    This is the query-layer rule of ADR-0037 items 5-7 in one pass: the media
    row is ready, the episode resolves to a canonical episode with a show, the
    show carries a label on the profile's region, and that label is at or under
    the cap. The ladder is the shipped snapshot, not a second copy.
    """
    regions = _region_ladder()
    rows = conn.execute(
        "SELECT l.item_key, m.metadata_status, ce.tmdb_show, ct.certifications_json "
        "FROM media_item_links l "
        "JOIN media_items m ON m.id = l.media_item_id "
        "LEFT JOIN metadata_canonical ce "
        "  ON ce.provider = 'tmdb' AND ce.entity_kind = 'episode' "
        " AND ce.provider_id = substr(l.item_key, 14) "
        "LEFT JOIN metadata_canonical ct "
        "  ON ct.provider = 'tmdb' AND ct.entity_kind = 'tv' "
        " AND ct.provider_id = CAST(ce.tmdb_show AS TEXT) "
        "WHERE l.item_key LIKE 'tmdb:episode:%' AND m.library_id = 1 "
        "ORDER BY l.item_key"
    ).fetchall()
    keys: list[str] = []
    for key, status, show, certs in rows:
        if status != "ready" or show is None or not certs:
            continue
        try:
            labels = json.loads(certs)
        except json.JSONDecodeError:
            continue
        label = labels.get(region) if isinstance(labels, dict) else None
        if not isinstance(label, str) or not label.strip():
            continue
        if _label_at_or_under_cap(regions, region, label.strip(), cap):
            keys.append(key)
    return keys


def seed_history(
    conn: sqlite3.Connection, profile_ref: str, cap: str, region: str
) -> int:
    """Write exactly HISTORY_ROWS eligible rows, so all of them are servable.

    Only keys the capped profile can see are chosen. That keeps the source and
    the eligible population the same size, and it means a manifest count of
    5000 history rows is also 5000 rows the rail can return.
    """
    profile_id = _profile_id(conn, profile_ref)
    # A duplicate media file shares its key, so collapse to one row per key.
    keys = list(dict.fromkeys(eligible_episode_keys(conn, cap, region)))
    if len(keys) < HISTORY_ROWS:
        _fail(
            f"library holds {len(keys)} eligible episode links; the "
            f"{HISTORY_ROWS}-row history needs at least that many"
        )
    keys = keys[:HISTORY_ROWS]
    conn.execute("DELETE FROM watch_state WHERE profile_id = ?", (profile_id,))
    rows = []
    for i, key in enumerate(keys):
        # Ascending, fixed timestamps; excluded from the manifest hash. The
        # format is a real ISO instant and sorts in the same order as `i`, so
        # the expected rail order is a pure function of the selected keys.
        last_played = (
            f"2026-01-01T{i // 3600:02d}:{(i // 60) % 60:02d}:{i % 60:02d}.000Z"
        )
        rows.append(
            (
                profile_id,
                key,
                1000 + (i % 60000),
                1800000,
                0,
                0,
                last_played,
                last_played,
            )
        )
    conn.executemany(
        "INSERT INTO watch_state (profile_id, item_key, position_ms, duration_ms, "
        "played, hidden, first_played_at, last_played_at) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        rows,
    )
    return len(rows)


def _series_index(conn: sqlite3.Connection) -> dict[str, int]:
    return {
        relpath: show_id
        for relpath, show_id in conn.execute(
            "SELECT relpath, tmdb_show_id FROM series WHERE library_id = 1"
        )
    }


def _folder_from_path(path: str) -> str:
    return path.split("/", 1)[0] if "/" in path else path


def verify_history(
    conn: sqlite3.Connection, profile_ref: str, cap: str, region: str
) -> dict:
    """Prove the seeded history against the disposable DB.

    Every field is an actual count read from the DB: the source population of
    episode links, the rows written for this profile, and the subset the capped
    profile can see. The expected rail is then derived from those eligible rows
    with the server's own collapse and order rules, so the driver can assert
    membership and order rather than only non-emptiness.
    """
    profile_id = _profile_id(conn, profile_ref)
    source_rows = int(
        conn.execute(
            "SELECT COUNT(*) FROM media_item_links l "
            "JOIN media_items m ON m.id = l.media_item_id "
            "WHERE l.item_key LIKE 'tmdb:episode:%' AND m.library_id = 1"
        ).fetchone()[0]
    )
    history_rows = int(
        conn.execute(
            "SELECT COUNT(*) FROM watch_state WHERE profile_id = ?", (profile_id,)
        ).fetchone()[0]
    )
    regions = _region_ladder()
    series = _series_index(conn)
    rows = conn.execute(
        "SELECT w.item_key, w.played, w.hidden, w.last_played_at, "
        "m.path, m.metadata_status, ce.tmdb_show, ct.certifications_json "
        "FROM watch_state w "
        "JOIN media_item_links l ON l.item_key = w.item_key "
        "JOIN media_items m ON m.id = l.media_item_id "
        "LEFT JOIN metadata_canonical ce "
        "  ON ce.provider = 'tmdb' AND ce.entity_kind = 'episode' "
        " AND ce.provider_id = substr(w.item_key, 14) "
        "LEFT JOIN metadata_canonical ct "
        "  ON ct.provider = 'tmdb' AND ct.entity_kind = 'tv' "
        " AND ct.provider_id = CAST(ce.tmdb_show AS TEXT) "
        "WHERE w.profile_id = ? GROUP BY w.item_key ORDER BY w.item_key",
        (profile_id,),
    ).fetchall()

    eligible: list[dict] = []
    for key, played, hidden, last_played, path, status, show, certs in rows:
        if status != "ready" or show is None or not certs:
            continue
        try:
            labels = json.loads(certs)
        except json.JSONDecodeError:
            continue
        label = labels.get(region) if isinstance(labels, dict) else None
        if not isinstance(label, str) or not label.strip():
            continue
        if not _label_at_or_under_cap(regions, region, label.strip(), cap):
            continue
        folder = _folder_from_path(path)
        show_id = series.get(folder)
        series_key = (
            f"tmdb:show:{show_id}" if show_id is not None else f"folder:1:{folder}"
        )
        eligible.append(
            {
                "item_key": key,
                "series_key": series_key,
                "played": bool(played),
                "hidden": bool(hidden),
                "last_played_at": last_played,
            }
        )

    groups: dict[str, list[dict]] = {}
    for row in eligible:
        if row["hidden"]:
            continue
        groups.setdefault(row["series_key"], []).append(row)

    expected: list[dict] = []
    for series_key, candidates in groups.items():
        in_progress = sorted(
            (c for c in candidates if not c["played"]), key=lambda c: c["item_key"]
        )
        if not in_progress:
            continue
        chosen = in_progress[0]
        for candidate in in_progress[1:]:
            if candidate["last_played_at"] > chosen["last_played_at"]:
                chosen = candidate
        group_last = max(c["last_played_at"] for c in candidates)
        expected.append(
            {
                "seriesKey": series_key,
                "itemKey": chosen["item_key"],
                "lastPlayedAt": group_last,
            }
        )
    expected.sort(key=lambda entry: entry["seriesKey"])
    expected.sort(key=lambda entry: entry["lastPlayedAt"], reverse=True)

    return {
        "source_history_rows": source_rows,
        "history_rows": history_rows,
        "eligible_rail_rows": len(eligible),
        "rail_expected": expected,
    }


def manifest(
    conn: sqlite3.Connection, seed: int, requested: int, profile_id: int | None = None
) -> dict:
    """Hash the deterministic columns only: no timestamps, no auth, no profile."""
    items = [
        list(row)
        for row in conn.execute(
            "SELECT path, title, kind, season, episode FROM media_items "
            "WHERE library_id = 1 ORDER BY path"
        )
    ]
    links = [
        list(row)
        for row in conn.execute(
            "SELECT media_item_id, item_key FROM media_item_links "
            "ORDER BY media_item_id, item_key"
        )
    ]
    series = [
        list(row)
        for row in conn.execute(
            "SELECT library_id, relpath, tmdb_show_id FROM series ORDER BY relpath"
        )
    ]
    canonical = [
        list(row)
        for row in conn.execute(
            "SELECT entity_kind, provider_id, title, season, episode, tmdb_show, "
            "certifications_json FROM metadata_canonical "
            "ORDER BY entity_kind, provider_id"
        )
    ]
    if profile_id is None:
        history = []
    else:
        history = [
            list(row)
            for row in conn.execute(
                "SELECT item_key, position_ms, duration_ms, played, hidden "
                "FROM watch_state WHERE profile_id = ? ORDER BY item_key",
                (profile_id,),
            )
        ]
    classifications = [
        list(row)
        for row in conn.execute(
            "SELECT s.relpath, CASE WHEN s.tmdb_show_id IS NULL THEN 'unmatched' "
            "ELSE 'bound' END FROM series s ORDER BY s.relpath"
        )
    ]
    content = {
        "items": items,
        "links": links,
        "series": series,
        "canonical": canonical,
        "history": history,
        "series_class": classifications,
    }
    content_sha256 = _sha256_text(_canonical_json(content))
    source_history_rows = int(
        conn.execute(
            "SELECT COUNT(*) FROM media_item_links l "
            "JOIN media_items m ON m.id = l.media_item_id "
            "WHERE l.item_key LIKE 'tmdb:episode:%' AND m.library_id = 1"
        ).fetchone()[0]
    )
    duplicate_items = int(
        conn.execute(
            "SELECT COUNT(*) - COUNT(DISTINCT item_key) FROM media_item_links"
        ).fetchone()[0]
    )
    if duplicate_items == 0:
        _fail("generator wrote no duplicate media items")
    counts = {
        "media_items": len(items),
        "links": len(links),
        "series_rows": len(series),
        "canonical_rows": len(canonical),
        "source_history_rows": source_history_rows,
        "history_rows": len(history),
        "duplicate_items": duplicate_items,
    }
    return {
        "generator": "perf_large_library.py",
        "seed": seed,
        "requested_items": requested,
        "counts": counts,
        "content_sha256": content_sha256,
    }


def cmd_generate(args: argparse.Namespace) -> int:
    data_dir = Path(args.data_dir).resolve()
    media_root = Path(args.media_root).resolve()
    seed_file = Path(args.seed_file).resolve()
    ensure_migrated(data_dir, Path(args.nightjar_bin))
    if media_root.exists():
        shutil.rmtree(media_root)
    media_root.mkdir(parents=True)
    plan = build_plan(args.items)
    _seed_files(plan, media_root, seed_file)
    conn = sqlite3.connect(data_dir / "nightjar.db")
    try:
        with conn:
            _seed_rows(conn, plan, media_root)
        document = manifest(conn, args.seed, args.items)
    finally:
        conn.close()
    document["manifest_sha256"] = _sha256_text(_canonical_json(document))
    output = Path(args.manifest_out).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    print(json.dumps(document, sort_keys=True))
    return 0


def cmd_seed_history(args: argparse.Namespace) -> int:
    data_dir = Path(args.data_dir).resolve()
    conn = sqlite3.connect(data_dir / "nightjar.db")
    try:
        with conn:
            written = seed_history(conn, args.profile_ref, args.cap, args.region)
    finally:
        conn.close()
    print(json.dumps({"history_rows": written}))
    return 0


def cmd_verify_history(args: argparse.Namespace) -> int:
    data_dir = Path(args.data_dir).resolve()
    conn = sqlite3.connect(data_dir / "nightjar.db")
    try:
        report = verify_history(conn, args.profile_ref, args.cap, args.region)
        output = Path(args.manifest_out).resolve()
        document = json.loads(output.read_text())
        # Rebuild the deterministic content hash now that the history rows
        # exist, then attach the counts the DB query just returned.
        document = manifest(
            conn,
            document["seed"],
            document["requested_items"],
            _profile_id(conn, args.profile_ref),
        )
        document["counts"]["source_history_rows"] = report["source_history_rows"]
        document["counts"]["history_rows"] = report["history_rows"]
        document["counts"]["eligible_rail_rows"] = report["eligible_rail_rows"]
        document["rail_expected"] = report["rail_expected"]
        document["manifest_sha256"] = _sha256_text(_canonical_json(document))
        output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    finally:
        conn.close()
    print(
        json.dumps(
            {
                "source_history_rows": report["source_history_rows"],
                "history_rows": report["history_rows"],
                "eligible_rail_rows": report["eligible_rail_rows"],
                "rail_expected": len(report["rail_expected"]),
            },
            sort_keys=True,
        )
    )
    if report["history_rows"] != HISTORY_ROWS:
        _fail(
            f"history_rows is {report['history_rows']}, expected {HISTORY_ROWS}"
        )
    if report["eligible_rail_rows"] != HISTORY_ROWS:
        _fail(
            f"eligible_rail_rows is {report['eligible_rail_rows']}, "
            f"expected {HISTORY_ROWS}"
        )
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    generate = sub.add_parser("generate", help="seed a deterministic library")
    generate.add_argument("--items", type=int, required=True)
    generate.add_argument("--data-dir", required=True)
    generate.add_argument("--media-root", required=True)
    generate.add_argument("--seed-file", required=True)
    generate.add_argument("--seed", type=int, default=1)
    generate.add_argument("--nightjar-bin", default="server/target/release/nightjar")
    generate.add_argument("--manifest-out", required=True)
    generate.set_defaults(func=cmd_generate)

    history = sub.add_parser("seed-history", help="write the 5000-row watch history")
    history.add_argument("--data-dir", required=True)
    history.add_argument("--profile-ref", required=True)
    history.add_argument("--cap", default=DEFAULT_CAP)
    history.add_argument("--region", default=DEFAULT_REGION)
    history.set_defaults(func=cmd_seed_history)

    verify = sub.add_parser(
        "verify-history",
        help="count the seeded rows against the DB and write the rail expectation",
    )
    verify.add_argument("--data-dir", required=True)
    verify.add_argument("--profile-ref", required=True)
    verify.add_argument("--cap", default=DEFAULT_CAP)
    verify.add_argument("--region", default=DEFAULT_REGION)
    verify.add_argument("--manifest-out", required=True)
    verify.set_defaults(func=cmd_verify_history)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
