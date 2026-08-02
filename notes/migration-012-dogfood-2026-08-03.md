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

Gate 3 live remount / Docker bind-path repoint remains a separate check.
