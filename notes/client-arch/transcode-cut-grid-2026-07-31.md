# Transcode cut grid (Step 1c)

- Date: 2026-07-31
- Tolerance: **50 ms** on sidx earliest_presentation_time
- Encode window: 40s; hls_time ∈ {2.0, 10.0}
- Args: libx264 ultrafast, `-force_key_frames expr:gte(t,n_forced*H)`,
  `-g 600 -keyint_min 48 -sc_threshold 0` (ADR-0008 §3 floor)
- Expected grid: `want[i] = start_s + i × hls_time` (title-absolute via
  `-output_ts_offset`). This is **not** the copy-mode KF-prediction
  instrument.

## Headline

| slice | compared | mismatches | rate |
|---|---:|---:|---:|
| all | 291 | 18 | 0.062 |
| mid-start only (`start_s > 0`) | 192 | **0** | **0.000** |
| start=0 only | 99 | 18 | 0.182 |

### By hls_time (all starts)

| hls_time | mismatches/compared | rate |
|---:|---:|---:|
| 2.0 | 15 / 242 | 0.062 |
| 10.0 | 3 / 49 | 0.061 |

### By shape (all starts)

| shape | mismatches/compared | rate |
|---|---:|---:|
| feature | 0 / 72 | 0.000 |
| VFR | 0 / 3 | 0.000 |
| long-GOP | 6 / 96 | 0.062 |
| short-GOP | 6 / 72 | 0.083 |
| damaged-DVD (8512) | 6 / 48 | 0.125 |

Every non-zero cell is a `start_s=0` run. Mid-title lands are clean at 50 ms
on every title and both `hls_time` values.

## What the residual is

At `start=0`, sidx times sit ~21 ms early at segment 0 and drift ~2 ms per
segment (40 s window max |Δ| ≈ 59 ms). That is priming / timebase skew, not
GOP packing. It is not the copy-mode multi-second failure mode.

## Verdict

**Yes for the load-bearing case.** Mid-start forced-IDR boundaries match the
uniform grid (0 / 192 at 50 ms). An honest full-title playlist is publishable
for transcode sessions; the scrubbing ceiling lifts on the bandwidth session
path (remote), where it still matters.

Start=0 carries a sub-100 ms cumulative skew. Treat as a known priming offset
to absorb in playlist `EXTINF` / tolerance, not as a reason to keep producer-
truth windowing for transcode.

Raw: `notes/client-arch/transcode-cut-grid-2026-07-31.json`.
Instrument: `scripts/t1c_transcode_cut_grid.py`.
