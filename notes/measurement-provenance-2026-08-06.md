# Measurement provenance — 2026-08-06 dogfood runs

Every figure cited by [ADR-0023](../docs/adr/0023-cluster-map-byte-offset-start.md),
[ADR-0041](../docs/adr/0041-subtitle-classification-and-client-gated-extraction.md),
and Rule 4.13's illustration traces to one of the runs below. Recomputed
directly from the committed CSVs on 2026-08-06 (after anonymisation — see
"Anonymisation" below; the transform does not touch any of these numbers).

**Transport, all runs:** SMB over WiFi to the household NAS, one disk (disk 8)
removed and the array rebuilding. Every timing figure below is a degraded-network
upper bound — re-measure once the rebuild finishes before citing these as
steady-state numbers.

## keyframe-index-2026-08-06.csv

- Script: `scripts/keyframe_index_probe.py --db ~/nightjar.db.copy --map ... --sample 300 --out keyframe-index-2026-08-06.csv`
- n = 300 (295 ok, 5 failed: 1 unrecognised container / non-media-index format,
  1 genuine `invalid EBML vint` damaged file — see 3 remaining failures in raw
  ffprobe/parse errors not otherwise classified)
- Matroska (n=263): index usable 100.0% (263/263), source `seekhead` on all.
  Build ms: median 130, p95 617, max 1,545. Bytes read: median 332,907, p95
  1,005,102, max 2,395,933. Coverage: median 99.9%, 2 of 263 under 98%.
- MP4 (n=32): index usable 100.0% (32/32), source `sample-tables` on all.
  Build ms: median 147, p95 340, max 414. moov position: end=24 (75.0%),
  faststart=8 (25.0%).
- Cites: ADR-0023 §2 ("measured 100% (295 of 295), n=300 ... median 130 ms
  (Matroska) / 147 ms (MP4)"), ADR-0023 §9 ("130 ms median build"), ADR-0023
  amendment consequences ("Index-usable rate corrected to 100% (n=300) ...
  MP4 faststart share corrected to 25% (n=32)").

## keyframe-fallback-2026-08-06.csv

- Script: same as above, `--fallback --sample 20 --out keyframe-fallback-2026-08-06.csv`
  (packet-walk timing added; small sample because it demuxes the whole file)
- n = 20, all ok. Packet walk: median 21,816 ms, max 261,128 ms. Fallback /
  index cost ratio: median 141x.
- Cites: ADR-0023 §2 ("Measured packet-walk cost where it does apply (n=20):
  21.8 s median, 261 s max, 141× the index cost").

## client-mix-2026-08-06.csv

- Script: `scripts/client_timeline_probe.py --db ~/nightjar.db.copy --map ... --sample 200 --out client-mix-2026-08-06.csv`
  (unbiased sample across the derived-method population, `--only all`)
- n = 200, all ok. Method mix: `transcode` 189 (94.5%), `remux` 8 (4.0%),
  `direct_play` 3 (1.5%). Standalone subtitle extract required: 0 of 200
  (0.0%). Index coverage under 98%: 1 of 200.
- Cites: ADR-0041 Context ("Client timeline, n=200 unbiased: standalone
  extraction was needed by 0 of 200 sessions").

## client-session-2026-08-06.csv

- Script: `scripts/client_timeline_probe.py --db ~/nightjar.db.copy --map ... --sample 60 --only session --out client-session-2026-08-06.csv`
  (restricted to the derived remux/transcode population)
- n = 60, all ok. Method mix: `remux` 2 (3.3%), `transcode` 58 (96.7%). No
  standalone extraction needed for any row (subtitles come from the session
  already reading the file).
- Supports ADR-0041 Decision 4 ("a remux or transcode session already runs
  ffmpeg reading the file ... produces the artifact as a side output").

## client-subs-2026-08-06.csv

- Script: `scripts/client_timeline_probe.py --db ~/nightjar.db.copy --map ... --sample 24 --only direct_play --subs --out client-subs-2026-08-06.csv`
  (restricted to `direct_play`; `--subs` times a real standalone WebVTT
  extract, so kept to a small sample deliberately)
- n = 24, all ok, 100% `direct_play`. Standalone extract required: 11 of 24
  (45.8%). Of those 11: extract wall median 5,002 ms (5.0 s), max 7,184 ms
  (7.2 s); source size median 0.23 GB (235 MB); implied throughput ≈55 MB/s.
- Cites: ADR-0041 Decision 4 ("Standalone extraction wall on that population
  (`--only direct_play`, n=24, 11 with text tracks): 5.0 s median, 7.2 s max,
  235 MB median source, 55 MB/s, zero failures").

## subtitle-inventory-2026-08-06.csv — not in this working tree

ADR-0041's Context bullet ("Subtitle inventory, n=500: 66.8% extract-class,
20.8% sidecar-only, 12.0% image-only, 0.4% no_subs") cites a run of
`subtitle_inventory_scan.py --sample 500`. **That CSV is not present in this
repository or working tree** — it was not found alongside the other five
2026-08-06 CSVs, committed or untracked, and no local copy was found outside
the repo either. The figure cannot be re-verified from a file at this time.

Flag for the human: either the file exists somewhere not checked here (a
different machine, a since-cleaned scratch directory) and should be located,
hashed with `--hash-paths`, and added to this repo under the same treatment
as the other five; or the number in ADR-0041 needs its own note pointing at
wherever the source run actually lives. Until one of those happens, treat
the 66.8/20.8/12.0/0.4% figures as reported-but-unverified-in-tree.

## Anonymisation

All five CSVs above originally carried the real filesystem `path` (library
mount, category folder, title, release-group/source tag in the filename) as
their first column. On 2026-08-06 (follow-up pass) that column was rewritten
in place — same rows, same order, same values in every other column — to a
non-reversible `lib{1|2}:{sha256(path)[:8]}` stand-in: `lib1` for the
`Movies` mount, `lib2` for `TV Shows`. No figure in this note or in ADR-0023
/ ADR-0041 depends on the `path` column; all of them are computed from
container, codec, size, stream-count, and timing columns, which are
untouched. `scripts/subtitle_inventory_scan.py`, `scripts/keyframe_index_probe.py`,
and `scripts/client_timeline_probe.py` all take `--hash-paths` now, so a
future run can produce hashed output directly instead of needing this
transform after the fact. Unhashed raw output should be named with a `-raw`
suffix, which `.gitignore` now excludes.
