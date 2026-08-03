# Metadata Block 1 dogfood — N150 (2026-08-04)

**Host:** `nightjar@192.168.1.183` (`nightjar-dev`)  
**Container:** `nightjar-dogfood` / `nightjar/nightjar:n150-hw`  
**Branch build:** `metadata/season-bind-and-product-drain` (rsync + docker build)  
**Data:** `~/gate2/data-docker-dogfood` (`NIGHTJAR_DATA_DIR=/config`)  
**UI:** http://192.168.1.183:8096  

## Deploy steps used

1. Rsync source → `~/gate2/src/nightjar` (no `.git` / targets)
2. Copy Mac `~/nightjar-data/secrets` → data dir (mode 600)
3. DB backup: `nightjar.db.bak-pre-metadata-20260803T202058Z` (via `docker exec` copy)
4. `docker build -t nightjar/nightjar:metadata-dogfood .` on box
5. Extract binary → `~/gate2/docker-hw/nightjar`, rebuild `n150-hw`
6. `bash ~/gate2/run-docker-dogfood.sh`

## Startup checks

| Check | Result |
|---|---|
| Health | `{"status":"ok","version":"0.0.1"}` |
| Migration | **13** applied (`cleaner_version`) |
| Preferred encoder | `h264_qsv` |
| Libraries | Movies 1749 + TV 23099, reachable |
| Secrets | present under `/config/secrets` |
| Metadata drain | started `pending=24848` |

## ~1 min after start (drain still running)

| Metric | Value |
|---|---:|
| `metadata_status=pending` | 23 619 |
| `ready` | 1 228 |
| `unmatched` | 1 |
| `media_item_links` | 1 230 |
| canonical movie / tv / episode | 9 / 40 / 1 231 |
| ready episodes unlinked | **1** |

Season→episode bind is live on product drain (episode rows + links tracking ready).

## Fix API smoke

`GET /api/v0/items/1/metadata/candidates` for *500 Days of Summer*:

- 1 candidate: TMDB movie **19913** *(500) Days of Summer* (2009)

## Leave running

Full first-run drain (with seasons) will take tens of minutes to ~hour on this library/IP budget. Watch:

```bash
ssh nightjar@192.168.1.183 'docker logs -f nightjar-dogfood 2>&1 | grep metadata'
```

When drain pass completes, look for `metadata drain pass complete` with `seasons_fetched` / `files_linked`.
