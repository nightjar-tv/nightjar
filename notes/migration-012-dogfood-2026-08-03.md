# Migration 012 dry-run — dogfood DB (2026-08-03)

VACUUM copy of `~/nightjar-data/nightjar.db`, then `nightjar_db::migrate`
through schema version 12 (ADR-0030 library-relative paths). Disposable copy
only; live dogfood was not rewritten.

| Metric | Before | After |
|---|---:|---:|
| `schema_migrations` max | 8 | 12 |
| `media_items` COUNT | 24940 | 24940 |
| `media_item_sidecars` COUNT | 8583 | 8583 |
| `SUM(libraries.paths_unresolved)` | — | 0 |
| Absolute-shaped item paths (`/` or `X:`) | — | 0 |
| Absolute-shaped sidecar paths | — | 0 |

Wall time ~0.8 s on the copy (673 MiB after VACUUM).

Repro:

```sh
sqlite3 "$NIGHTJAR_DATA_DIR/nightjar.db" "VACUUM INTO /tmp/nj-mig012.db"
NIGHTJAR_MIGRATE_COPY=/tmp/nj-mig012.db cargo test -p nightjar-db \
  migrate_copy_through_012 -- --ignored --nocapture
```

Gate 3 remount: see `notes/gate3-repoint-dogfood-2026-08-03.md`.

## Live migrate (2026-08-03)

Same host DB (`~/nightjar-data/nightjar.db`), not a copy. Binary from
`main` @ `18ae3c1` (#43 resolve + requeue; ADR-0030 already on main via #33).

1. Server stopped (no nightjar process).
2. Backup: `VACUUM INTO
   ~/nightjar-data/backups/nightjar-pre-012-20260803T085722Z.db` (673 MiB;
   schema 8, 24940 items).
3. Start `server/target/release/nightjar` with
   `NIGHTJAR_DATA_DIR=/Users/gmacarthur/nightjar-data` — product startup
   migrate path (migrations 9–12).

| Metric | Before | After |
|---|---:|---:|
| `schema_migrations` max | 8 | 12 |
| `media_items` COUNT | 24940 | 24940 |
| `media_item_sidecars` COUNT | 8583 | 8583 |
| `SUM(libraries.paths_unresolved)` | — | 0 |
| Absolute-shaped item paths | — | 0 |
| Absolute-shaped sidecar paths | — | 0 |

Log: applied versions 9, 10, 11, 12 (~0.25 s strip after DDL). Sample
stored paths are relpaths (`av1_aac_mp4.mp4`,
`(500) Days of Summer (2009)/…mkv`).

Spot-check after resolve (#43): join `libraries.path` + stored relpath opens
item 32 on SMB; `GET /api/v0/items/32` reconstructs absolute
`MediaItem.path`; `playback-info` returns duration + 2 soft subtitle tracks
(`subtitleStatus=ready`).
