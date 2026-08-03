# Scanner warm-walk breakdown (E0) — N150 2026-08-03

Measure residual after #44–#46 on the dogfood host
(`nightjar@192.168.1.183`). Goal: separate **directory-mtime walk** cost from
**per-file path resolve** and **DB fold-match** on an unchanged library.

Harness: `scripts/scan_warm_breakdown.py` (serial walk, same media-ext and
dir-mtime cache idea as ADR-0013). Product default walk concurrency is **8**,
so product **walk** wall should be lower than serial walk numbers; per-file
canonicalize is largely serial in the index loop today.

Host paths: `/mnt/media/Movies`, `/mnt/media/TV Shows` (same tree as
container `/media/...`). DB: `~/gate2/data-docker-dogfood/nightjar.db`
(library_id 1 Movies, 2 TV). Quiet host measure (not inside the product
process; no concurrent probe load).

## Results

### Movies (1 749 files, 1 765 dirs)

| Leg | ms | Notes |
|---|---:|---|
| Cold walk | **261 170** (~4.4 min) | all dirs relisted |
| Warm walk | **6 146** (~6.1 s) | 0 relisted; dir stats only |
| Canonicalize all files | **7 904** (~7.9 s) | 1749 `Path.resolve` |
| resolve + relpath + fold | **3 649** | no DB |
| DB list paths | **10** | 1749 rows |
| fold-match + mtime | **1 838** | walk-adjacent; see caveat |
| **Warm walk + canon + match** | **~15 888** (~15.9 s) | |

Share of warm sum (walk + canon + match):

| Leg | % |
|---|---:|
| Warm walk | **38.7%** |
| Canonicalize | **49.7%** |
| Fold-match | **11.6%** |
| DB list | ~0% |

### TV (23 099 files, 3 056 dirs)

| Leg | ms | Notes |
|---|---:|---|
| Cold walk | **1 655 690** (~27.6 min) | serial full readdir |
| Warm walk | **1 837** (~1.8 s) | 0 relisted; fewer dirs than Movies |
| Canonicalize all files | **13 514** (~13.5 s) | 23099 resolves |
| resolve + relpath + fold | **72 595** (~72.6 s) | extra resolve work |
| DB list paths | **48** | 23099 rows |
| fold-match + **re-stat** | **870 276** (~14.5 min) | **measurement artifact** — see below |

**Caveat (TV fold_match):** the harness re-`stat`’d every media file during
match. Product uses **mtime from the walk** (`MediaFile.mtime_ms`) and does
not re-stat for the unchanged short-circuit. Treat TV `fold_match_ms` as an
upper bound on “stat every file again,” **not** as product match cost.

Product-like warm index estimate (walk mtimes, one canonicalize pass, no
re-stat):

| Lib | Warm walk | Canonicalize | DB + match (no re-stat) | Serial sum (approx) |
|---|---:|---:|---:|---:|
| Movies | ~6 s | ~8 s | ~2 s | **~16 s** |
| TV | ~2 s | ~14 s | small + resolve in loop | **~20–90 s** depending on whether resolve is paid once in the index loop |

Product concurrent walk (c=8) can cut the **walk** leg further when the share
is quiet; it does not remove per-file canonicalize in `run_index_pass`.

## Reads

1. **Warm dir-mtime cache works.** Movies cold → warm is ~40×; TV cold → warm
   is ~900× on this tree (3k dirs vs 23k files: warm cost tracks **dirs**,
   not files).

2. **On a quiet Movies warm path, canonicalize ≈ walk** (~half the synthetic
   index sum). DB fold-match is not the hotspot.

3. **TV warm walk is cheap (~2 s serial)** when the cache is hot. Multi-minute
   product `index_duration_ms` with `relisted_dirs=0` during the pre-#46
   treadmill was therefore **not** “must readdir 23k files.” Suspect share
   contention (probe/extract concurrent), epoch queueing, or cache not
   actually warm after root/path churn — not SQLite.

4. **Cold walk remains expensive** (Movies ~4 min, TV ~28 min serial). That
   is remount / first-boot / empty-cache cost. Slice C (repoint walk reuse)
   correctly avoids paying cold **twice** on repoint.

5. **E1 candidates (only if product still slow when quiet):**
   - Avoid or batch **canonicalize** when the walk path is already under the
     library root (keep ADR-0030 symlink-escape safety).
   - Keep **dirty coalesce (#46)** so warm polls are rare and not stacked.
   - Do **not** optimise DB match first.

## Repro

```sh
# on N150 (script also in repo: scripts/scan_warm_breakdown.py)
python3 ~/gate2/scan_warm_breakdown.py \
  --root /mnt/media/Movies \
  --db ~/gate2/data-docker-dogfood/nightjar.db \
  --library-id 1 --label Movies

python3 ~/gate2/scan_warm_breakdown.py \
  --root "/mnt/media/TV Shows" \
  --db ~/gate2/data-docker-dogfood/nightjar.db \
  --library-id 2 --label TV
```

## Related

- `notes/scanner-residuals-2026-08-04.md` — residual queue (E0)
- `run_index_pass` — `server/crates/scanner/src/lib.rs`
- Walk cache — `server/crates/scanner/src/walk.rs`
