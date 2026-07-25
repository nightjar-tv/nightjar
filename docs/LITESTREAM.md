# Litestream backup for Nightjar SQLite

Nightjar stores state in a single SQLite file:

```
$NIGHTJAR_DATA_DIR/nightjar.db   # default: ./data/nightjar.db
```

WAL mode is enabled (`nightjar.db-wal` / `nightjar.db-shm` may appear beside it).
That second file next to the database is normal.

## Why Litestream

Litestream streams SQLite WAL pages to object storage so a crashed disk is not a
lost library. It is an external companion process, not linked into the Nightjar
binary (Rule 1.2).

## Minimal config

```yaml
# litestream.yml
dbs:
  - path: /config/nightjar.db
    replicas:
      - type: s3
        bucket: your-bucket
        path: nightjar
        region: ap-southeast-2
```

Run alongside Nightjar (same volume mounts for `/config`):

```bash
litestream replicate -config litestream.yml
```

Restore before starting Nightjar after data loss:

```bash
litestream restore -config litestream.yml -o /config/nightjar.db
```

## Notes

- Point Litestream at the same path Nightjar uses (`NIGHTJAR_DATA_DIR`).
- Do not copy the `.db` file while Nightjar is running; use Litestream or
  `sqlite3 .backup` instead.
- Docker Compose examples that pair both processes land with install docs in
  Phase 4.
