# Client engine bake-off

Status: Steps 1 / 1b / 1c held. Step 2 (profile ADR + N100 capacity) and
Step 3 (ABR throttle) not started.
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

Bitrate is **not stored** per item. Today's filter is `size_bytes × 8 /
duration_ms`, which matches `ffprobe format.bit_rate` at ratio **1.000** on a
30-file sample (FFmpeg derives the same number when the container has no
declared rate). Fine for a library histogram; wrong as a permanent gate: a
title with a quiet 90 minutes and a loud 10 is misjudged either way. When the
profile ADR lands, store `format.bit_rate` from a header-only ffprobe at scan
(migration column + probe field; not a packet walk).

Instrument: `scripts/t1b_bitrate_shape.py`.
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

Compatibility going to zero does not make sessions rare for remote. The
**ceiling choice is the decision**, not an implementation detail:

| Default / Auto ceiling | Library share forced to bandwidth-transcode |
|---|---:|
| 4 Mbps | **49%** — half the library is a session |
| 8 Mbps | **21%** — common path for remote |
| 15 Mbps | **2%** — sessions are a genuine exception |
| 25 Mbps | **0.2%** |

Say that out loud in the profile ADR. Picking the Auto default picks the CPU
load.

---

## Step 1c — Transcode cut predictability (hold this)

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

**Held outcome:** zero mismatches on 192 mid-start cases means honest
full-title playlists are publishable for the only session path that survives
under an engine (bandwidth-transcode). Full seekable range, native scrubbers
working, no client compensation. The copy-mode scrubbing ceiling from this
week does **not** apply to the architecture we are moving to.

Start=0 at max |Δ| ≈ 59 ms is priming skew, not a blocker. Two easy answers:
measure the actual first-segment start and publish it as the first `EXTINF`,
or set `#EXT-X-START` to the real media start.

Detail: `notes/client-arch/transcode-cut-grid-2026-07-31.md`.

---

## Decision (current)

**What the engine settles**

- Compatibility: `MPV_V0` / `VLC_V0` = **100%** direct play on this library
  (T1). Container-remux for AVPlayer is optional architecture, not destiny.
- Path: ship Phase 4 on a Matroska-capable engine (libmpv / Rule 2.4). Do not
  treat Apple AVPlayer as the product path for the household library.
- Transcode sessions get honest full-title playlists (1c). Native scrubbers
  work on the path that remains.

**What it does not settle**

- Bandwidth: ceiling choice sets whether sessions are 49% / 21% / 2% of the
  library. Sessions remain the remote/bitrate path.
- ABR quality under libmpv on a throttled multi-variant playlist (Step 3):
  the only scenario that partially reopens platform players.

**Server consequence**

- Session pipeline stays. It stops being "AVPlayer refused the container"
  and becomes "link cannot carry the bitrate" (plus burn-in, outliers, web).
- Restart-latency backlog: **matters for remote**, not for ~80% of LAN
  engine playback. Refiled in `notes/session-latency-and-disk-backlog.md`.
  Do not fully demote it; remote viewers are least tolerant of a multi-
  second seek.
- Keyframe-map slice and ADR-0020 stand. Copy/remux keeps producer-truth
  windowing; under an engine that path is no longer the common LAN case.

### Step 2 — Profile ADR (requirements locked from this bake-off)

One ADR covering bitrate, resolution, and HDR on `ClientCapabilityProfile`
(ADR-0008 §6 / V1_PLAN Phase 2). Wire shape and DB fields before writers
(Rule 4.9), including a real `format.bit_rate` column from scan-time header
probe — do not ship size×8/duration as the permanent gate.

The ADR must reckon with:

1. **Remote is not one number.** A phone on LTE and a laptop on fibre both
   look "remote." A single ceiling either transcodes needlessly or does not
   help. Phase 3 local-versus-remote detection does not erase that; even
   "remote" needs a policy, not one magic Mbps.
2. **v1 ceiling is user-chosen** through Auto / High / Original (ADR-0008
   §1). Local-versus-remote detection is Phase 3; until then any remote cap
   is advisory. The **Auto default** is what actually sets CPU load. State
   explicitly: **15 Mbps as Auto default → sessions ~2% (exception);
   4 Mbps → ~49% (half the library is a transcode).**
3. **`decide_playback` when only bitrate fails:** transcode with copyable
   audio, own reason string (not "video codec unsupported").
4. **N100 capacity measurement (add to Step 2, before calling Gate 2 sized):**
   how many concurrent bandwidth transcodes an Intel N100 sustains at the
   chosen Auto target (1080p). Gate 2 already asks for three simultaneous
   1080p; 21% or 2% of playback is what lands there depending on the
   ceiling. Convert the percentage into a hardware answer.

### Step 3 — still open

libmpv / `media_kit` vs hls.js on the same multi-variant playlist under
throttle. Only partial reopen of platform players.

### Phase 4 / ADR-0020

- Phase 4 player-core matches the constitution. ADR-0001 should be accepted
  on that basis.
- ADR-0020 stands for copy/remux sessions. Transcode may publish honest
  full-title playlists (1c).
- T2/T3/T4 confirmation on a laptop prototype when Phase 4 starts.
