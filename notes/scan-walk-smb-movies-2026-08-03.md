# Movies SMB index walk (2026-08-03)

Live corpus-data server (`NIGHTJAR_DATA_DIR=~/nightjar-corpus-data`),
library Movies → `/Volumes/media/Movies` over SMB.

| Job | Result | `index_duration_ms` | Notes |
|---|---|---:|---|
| 886 | `removed=24`, `unchanged=1749` | **594,759** (~595 s) | First index after kit tree moved out of Movies; 1,773 rows at start |
| 888–895 | warm | 36–58 s | `unchanged=1749` |

~595 s / 1,773 items ≈ **0.34 s/item**. Same latency-bound smell as the
old serial directory walk (22 s → 1.7 s once parallelised). Not checked
here whether this path is already at walk concurrency or still serial on
some step. Measurement only — not a Block 1 slice; does not block metadata.

## delete_missing gated on full walk

Removals only land when the index pass finishes a complete tree walk.
Deletes cannot be cheap while that gate holds. Design observation for
later; do not fix in Block 1.

## Kit move (same morning)

`Movies/dolby-vision-browser-kit/` → `/Volumes/media/dolby-vision-browser-kit`
(DV library root / measure-exclude). Job 886 is what cleared the 24
stale Movies rows.
