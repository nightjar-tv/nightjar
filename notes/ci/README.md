# CI gate metrics baseline

`scripts/ci_benchmark_summary.py` compares each gate1 run to
`gate-metrics-baseline.json` when that file exists. Until it does, the job
summary says **no baseline** — that is honest incomplete work, not a finished
Phase 2 item (Rule 4.8).

## Fill from the first green `main` run after the delta job lands

1. Open the successful `gate1` job on `main`.
2. Download the `gate-metrics` artifact (`/tmp/gate-metrics.json` contents).
3. Commit it here as `gate-metrics-baseline.json` (numbers only; drop any
   wrapper). Subject: `ci: seed gate metrics baseline from main`.
4. Note that commit in the PR / slice close-out that claimed the benchmark
   delta item. Do not call the item done while this file is missing.
