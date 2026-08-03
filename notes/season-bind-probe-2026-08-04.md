# Season→episode bind probe (2026-08-04)

**Mode:** `QUEUE_MAX_GROUPS=5`, `EXCLUDE_TESTDATA=1`, measure DB copy.  
**Binary:** `metadata-queue-measure` after bind metrics + product drain slice.  
**Not** a full-library Gate 3 figure — proof that live season fetch runs and links files.

## Result

| Metric | Value |
|---|---:|
| Show groups | 5 |
| Items ready | 119 |
| HTTP requests | 24 |
| Mean HTTP / group | **4.8** (vs ~1.8 movie+show-only model) |
| `seasons_in_drain` | **true** |
| Seasons fetched | 14 |
| Episodes projected | 129 |
| Files linked | 119 |
| Seasons skipped | 0 |
| Bind errors | 0 |
| Ready episodes unlinked | **1** |

## Reading

- Live `TmdbClient` path **does** call season detail during bind (ADR-0029 §3).
- Mean HTTP/group jumped from the ~1.84 search+detail model to **4.8** on this 5-show probe (extra season fetches).
- One ready-but-unlinked episode on this sample — **not a missing season stub.** Row was
  `path=dolby-vision-makemkv/P81_GlassBlowing2_….mkv`, `season=40`, `episode=216`
  (parser noise / kit file hanging under a show library or soft-key group). Full drain
  should bucket unbound ready by reason (null S/E, S/E miss, kit path).

## Unit proof

`drain_binds_episode_keys_when_source_returns_season` (season-returning double) asserts `tmdb:episode:{id}` links + episode canonical rows. Stub sources still skip seasons (`seasons_skipped`).

## Next

1. Full `QUEUE_FIRST_SCREEN=0` dogfood drain (with seasons) for req/1k + unlinked rate.
2. Sample the single unlinked row class on a larger pass.
3. Product `metadata-drain` thread now runs in `nightjar` (own SQLite connection).
