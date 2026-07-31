# Client engine bake-off

Status: Step 1 (compatibility) + 1b (bandwidth) + 1c (transcode grid) reported.
Profile-shape ADR (Step 2) and ABR throttle test (Step 3) not started.
Date: 2026-07-31

Framing: decision note for Phase 4 player architecture (engine vs platform
players). Not a Phase 4 code slice. ADR-0020 and the keyframe-map work stay
on their own track either way.

Context that forced this: copy-mode HLS boundaries are producer-owned
(ADR-0020). A stock scrubber can only seek the produced window. Plex / Emby /
Jellyfin share that defect on their session path; they avoid it by direct-
playing Matroska. Under Apple AVPlayer we mostly remux because AVPlayer will
not open MKV. Re-price that choice with numbers, once.

**Quote correctly:** under an engine profile, *compatibility*-transcode goes
to ~zero (codecs/containers the client cannot decode). *Bandwidth*-transcode
is a different axis. libmpv and VLC both play HLS; the session pipeline
survives as the path for bitrates the link cannot carry, which is what it
should always have been for (V1_PLAN Phase 2 / ADR-0008 §6).

---

## Step 0 — Decision rule (locked before measurement)

Results after this section are scored against these numbers. Do not move a
threshold to fit a result. If a threshold is wrong, name which one and why in
this document, then re-score; do not silently revise.

### T1 — Direct-play share (sessions become the exception)

| Gate | Threshold |
|---|---|
| Sessions are an exception path | direct-play ≥ **90%** of the dogfood library under that profile |
| Step 1 alone is decisive for an engine | `MPV_V0` or `VLC_V0` direct-play ≥ **90%** |
| Engine does not buy enough to justify itself on share alone | both engines direct-play **< 70%** |

Between 70% and 90%: Step 1 is inconclusive on share; continue to Step 2/3.

Consequence: at ≥90% *compatibility* direct play, copy/remux-for-container
stops being the common LAN path. Bandwidth-transcode rate is a separate
number (Step 1b); restart-latency work is refiled as remote/session, not as
"84.5% of playback" (see `notes/session-latency-and-disk-backlog.md`).

### T2 — Far-seek latency (user-visible, actual client)

Measured on the same titles as Step 2, warm and cold labelled, n≥10, spread
reported. Land = first decoded frame after scrub release.

| Gate | Threshold |
|---|---|
| Current session path is unacceptable | warm median far-seek (75% land) **> 3.0 s** |
| Engine justifies itself on latency | warm median far-seek **≤ 0.75 s**, and cold median **≤ 1.5 s** |

Near-seek (±30 s) is recorded but does not decide alone.

### T3 — Ownership cost (lines we write and keep)

Count lines written for the prototype, and estimate lines we would own per
platform at ship (Flutter shell + FFI glue + OSD/scrubber + platform video
surface). Upstream `media_kit` / libmpv lines are dependency weight, not
ownership, but forked or patched upstream counts as owned.

| Gate | Threshold |
|---|---|
| Engine too expensive to own | **> 2 500** lines of Nightjar-owned native/platform code **per platform** (Android, iOS, tvOS counted separately), excluding generated bindings |

Per-platform OSD/scrubber work is unavoidable under every outcome (no stock
scrubber can seek a producer-truth playlist). That cost is shared; T3 is the
extra engine/surface cost beyond OSD.

### T4 — Failure rate on the real library

Titles the profile classified as direct-play that refuse, crash, or fail to
reach first frame in the engine under test.

| Gate | Threshold |
|---|---|
| Engine disqualified | failure rate **> 2.0%** of profile-claimed direct-play titles on the dogfood library (or on the Step 2/3 title set if the full library cannot be walked) |

Damaged titles (e.g. 8519) are scored separately as behaviour notes, not as
automatic failures, when the file itself is truncated or demux-hostile.

### T5 — Keep the current architecture unchanged

Keep platform players (Apple AVPlayer / Android Media3 / web) and treat the
session path as the main product path when **any** of:

1. Both `MPV_V0` and `VLC_V0` direct-play **< 70%** (T1), or
2. An engine clears T1 but fails T4, or
3. An engine clears T1 and T2 but breaches T3 with no path under the line budget, or
4. Step 2 shows Jellyfin's session-path far-seek is in the same band as ours
   (±20% warm median) **and** Step 1 engine share is < 90% — meaning we are
   not behind on session engineering; we are behind on direct-play exposure,
   but no engine clears the bar either.

If (4) holds with engine share ≥90%, we still take the engine: the fix is
direct play, not a better session.

### Decision procedure

1. Run Step 1. If `MPV_V0` or `VLC_V0` ≥ 90% direct play → provisional engine
   win on *compatibility* share; **stop and report** before the Infuse/Jellyfin
   evening. Steps 2–3 of the original bake-off become confirmation/cost.
2. Measure bandwidth shape (1b) and transcode-cut predictability (1c) before
   the profile ADR.
3. Final decision names the path, the thresholds that fired, cost, what we
   give up, and Phase 4 / ADR-0020 consequences.

### Threshold-change log

(none)

---

## Step 1 — Compatibility (codecs / containers)

Script: `scripts/t1_profile_counts.py`. Decision rules match
`nightjar-core::decide_playback`.

Database: `/Users/gmacarthur/nightjar-data/nightjar.db`  
Items: **24 877**

| Profile | directPlay | remux | transcode |
|---|---:|---:|---:|
| `APPLE_AVPLAYER_V0` | 3 214 (**12.9%**) | 19 879 (**79.9%**) | 1 784 (7.2%) |
| `ANDROID_MEDIA3_V0` | 24 014 (**96.5%**) | 2 (0.0%) | 861 (3.5%) |
| `MPV_V0` | 24 871 (**100.0%**) | 0 | 6 (0.0%) |
| `VLC_V0` | 24 871 (**100.0%**) | 0 | 6 (0.0%) |

Profile floors (capability, not optimism):

- `APPLE_AVPLAYER_V0`: H.264/HEVC + AAC/AC-3/E-AC-3/MP3/ALAC in MP4/MOV only.
  No Matroska. No DTS/TrueHD/FLAC/Opus/Vorbis. No AV1/VP9/MPEG-4.
- `ANDROID_MEDIA3_V0`: Matroska/WebM/MP4/AVI; H.264/HEVC/VP9/AV1/MPEG-4;
  AAC/AC-3/E-AC-3/MP3/Opus/FLAC/Vorbis. No DTS/TrueHD (licence/device split).
- `MPV_V0` / `VLC_V0`: Matroska plus the brief set (HEVC, AV1, VP9, DTS,
  TrueHD, FLAC, Opus) and the usual demux peers. PGS/ASS are subtitle
  capabilities; they do not change `decide_playback`.

Prior cited figures for Apple (13.1% / 84.5%) were from an earlier library
state. Current count is 12.9% / 79.9%. Same shape: under AVPlayer, sessions
are the common path because MKV is the library default.

The six non-direct-play rows under `MPV_V0`/`VLC_V0` are probe failures or
codecs outside the floor. Not a session tax.

### T1 score

- `MPV_V0` / `VLC_V0` = **100.0% ≥ 90%** → compatibility decisive.
- `ANDROID_MEDIA3_V0` already **96.5%**: Matroska demux, not an engine.
- `APPLE_AVPLAYER_V0` at 12.9% keeps container-remux as the main Apple path.

**This number is compatibility only.** It does not settle bandwidth.

---

## Step 1b — Bandwidth shape

Bitrate is **not stored** per item. Cost to add: one format-only ffprobe field
at scan (header read, not a packet walk), plus a migration column. On this
corpus, `ffprobe format.bit_rate` equals `size_bytes × 8 / duration_ms` at
ratio **1.000** (n=30 sample), so the distribution below is that format rate
without a multi-hour NAS walk. Instrument: `scripts/t1b_bitrate_shape.py`.
Raw: `notes/client-arch/bitrate-shape-2026-07-31.json`.

n = **24 842** (probed, positive duration/size, excluding `/testdata/`).

| | Mbps |
|---|---:|
| min | 0.07 |
| p50 | **3.86** |
| p90 | **10.01** |
| p99 | **18.24** |
| max | 42.24 (Remux/Bluray outliers; not the testdata 4 GiB stub) |

Histogram (selected buckets):

| Mbps | n | % |
|---|---:|---:|
| 0–2 | 3 647 | 14.7 |
| 2–4 | 9 044 | 36.4 |
| 4–8 | 6 908 | 27.8 |
| 8–15 | 4 779 | 19.2 |
| 15–25 | 445 | 1.8 |
| ≥25 | 46 | 0.2 |

### Share exceeding candidate ceilings (= remote transcode rate if that ceiling is the cap)

| Ceiling | Exceed n | Exceed % |
|---:|---:|---:|
| 4 Mbps | 12 178 | **49.0%** |
| 8 Mbps | 5 270 | **21.2%** |
| 15 Mbps | 491 | **2.0%** |
| 25 Mbps | 46 | **0.2%** |

By resolution (p50 / share >8 Mbps): 1080p n=18 829 p50=3.96 >8=17.3%;
720p n=4 914 p50=4.71 >8=40.7%; 2160p+ n=6 all above 15 Mbps.

By source tag: WEB p50=5.10 >8=25.2%; Bluray p50=3.18 >8=20.3%; Remux n=5
p50=34.5 all above 15 Mbps.

### How this differs from the compatibility number

| Axis | What it counts | Scale |
|---|---|---|
| Compatibility-transcode | codecs/containers the client cannot decode | Library property; counted once. Engine → ~0%. |
| Bandwidth-transcode | titles above the link ceiling | Scales with **concurrent remote viewers** and the chosen ceiling. At 8 Mbps on this library: **21.2%**. At 15 Mbps: **2.0%**. |

Compatibility going to zero does not make sessions rare for remote. Gate 2
CPU sizing depends on the bandwidth number and concurrency, not on the 100%
directPlay headline.

At an 8 Mbps advisory remote ceiling, sessions are common enough that
restart-latency and transcode capacity stay first-class. At 15 Mbps they are
an exception (~2%). The profile ADR (Step 2) has to pick the v1 ceiling
knowing local-vs-remote detection is Phase 3 and any remote cap is advisory
until then (ADR-0008 §6).

---

## Step 1c — Transcode cut predictability

Instrument: `scripts/t1c_transcode_cut_grid.py` (forced-IDR grid vs sidx,
not the copy-mode KF predictor). Titles: Elementary 3x05, Rick and Morty
9x04, 12 Angry Men, Futurama 4x06 (8512), corpus VFR. Starts include 0 and
mid-title. `hls_time` ∈ {2, 10}. Production-like `-force_key_frames` +
`-sc_threshold 0`.

| slice | mismatches / compared | rate |
|---|---:|---:|
| all | 18 / 291 | 0.062 |
| **mid-start only** | **0 / 192** | **0.000** |
| start=0 only | 18 / 99 | 0.182 |

Start=0 residual is ~21 ms at seg0 drifting ~2 ms/seg (max |Δ| ≈ 59 ms in a
40 s window). Priming/timebase, not GOP packing. Mid-title is clean at 50 ms
on every shape and both `hls_time` values.

**Verdict:** the scrubbing ceiling **lifts for transcode sessions** on the
load-bearing case (mid-title / far seek). An honest full-title playlist is
publishable for forced-IDR transcode. Absorb start=0 sub-100 ms skew in
tolerance; do not keep producer-truth windowing for transcode because of it.

Detail: `notes/client-arch/transcode-cut-grid-2026-07-31.md`.

---

## Decision (current)

**What the engine settles**

- Compatibility: `MPV_V0` / `VLC_V0` = **100%** direct play on this library
  (T1). Container-remux for AVPlayer is optional architecture, not destiny.
- Path: ship Phase 4 on a Matroska-capable engine (libmpv / Rule 2.4). Do not
  treat Apple AVPlayer as the product path for the household library.

**What it does not settle**

- Bandwidth: at **8 Mbps**, **21.2%** of the library still needs a session if
  that is the remote ceiling; at **15 Mbps**, **2.0%**. Sessions remain the
  remote/bitrate path. Gate 2 CPU sizing follows this axis and concurrency.
- ABR quality under libmpv on a throttled multi-variant playlist (Step 3):
  the only scenario that partially reopens platform players.

**Scrubbing ceiling (1c)**

- Lifts for **transcode** sessions (honest full-title grid; mid-start 0/192).
- Remains for **copy/remux** (ADR-0020 producer-truth). Under an engine,
  copy/remux is no longer the common LAN path; bandwidth-transcode is.

**Server consequence**

- Session pipeline stays. It stops being "AVPlayer refused the container"
  and becomes "link cannot carry the bitrate" (plus burn-in, outliers, web).
- Restart-latency backlog: **matters for remote**, not for ~80% of LAN
  engine playback. Refiled in `notes/session-latency-and-disk-backlog.md`.
  Do not fully demote it; remote viewers are least tolerant of a multi-
  second seek.
- Keyframe-map slice and ADR-0020 stand.

**Still open (before calling architecture fully settled)**

1. **Step 2 — profile ADR:** add bitrate, resolution, HDR to
   `ClientCapabilityProfile` (one ADR). Wire shape + DB fields before writers
   (Rule 4.9). Cover v1 advisory ceiling without Phase 3 local/remote detect;
   interaction with Auto/High/Original (ADR-0008 §1); `decide_playback` reason
   when only bitrate fails (transcode, copyable audio).
2. **Step 3 — ABR under engine:** libmpv/`media_kit` vs hls.js on the same
   multi-variant playlist under throttle. Only reopen of platform players.
3. T2/T3/T4 confirmation on a laptop prototype when Phase 4 starts.

**Phase 4 / ADR-0020**

- Phase 4 player-core matches the constitution. ADR-0001 should be accepted
  on that basis.
- ADR-0020 stands for every session that still runs.
- If Step 2 picks ~8 Mbps as the remote advisory floor, treat Gate 2
  concurrent-transcode capacity as a first-class follow-on. If it picks
  ~15 Mbps, bandwidth sessions are ~2% and capacity pressure is lower.
