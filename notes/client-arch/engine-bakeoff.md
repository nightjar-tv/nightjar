# Client engine bake-off

Status: Bake-off completion pass. Binding + AetherEngine scored; decision
still **provisional**. media_kit has **no tvOS plugin** (stock Flutter or
fluttertv/flutter-tvos; 2026-07-31). Step 2 profile ADR + Apple TV Part C
(Aether vs native) + contiguous full-title VOD remain open. T5-4 unscored.
Date: 2026-07-31

Framing: decision note for Phase 4 player architecture (engine vs platform
players). Not a Phase 4 code slice. The prototype under `apps/engine-bakeoff/`
is measurement-only; Rule 2.4’s single Rust/libmpv core wording is exactly
what the client ADR will supersede once this bake-off names the path.
ADR-0020 and the keyframe-map work stay on their own track either way.

**Pre-code T3 finding:** no maintained libVLC Flutter plugin for macOS
(`flutter_vlc_player` is mobile-only; `dart_vlc` is abandoned for Apple).
A bake-off FFI against VLC.app’s `libvlc.dylib` is required to measure
libVLC at all. That ownership tax exists before any latency number and can
outweigh latency when choosing an engine.

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

#### T2 gate 1 — scored (copy-mode baseline)

Pre-committed first-pass from `scripts/far_seek_baseline.mjs` /
`nightjar-meta/notes/far-seek-baseline-2026-07-31.md`:

| | |
|---|---|
| Path | **Copy-mode HLS** (ADR-0020 producer-truth), warm `POST /seek` → first listed segment 200 |
| n | 21 successful lands (cold not in this sample) |
| warm p50 | **4621 ms** (min 1596 / p90 8278 / max 9094) |
| Eviction | 0 |

**Score:** warm median **> 3.0 s** → gate 1 fires: that session path was
unacceptable.

**Scope (do not misread):** this measured **copy/remux HLS far-seek**. Under
an engine that path is nearly gone (Matroska direct play). The honest line is
“the path this measured is being deleted; the surviving transcode path is
unmeasured until the Part B prototype numbers.” Do not cite 4.62 s later as
“sessions are unacceptable” after the architecture has already removed copy
as the common LAN path.

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

#### T5 condition 4 — unscored (open)

Jellyfin same-box comparison is deferred (spinning up Jellyfin + forcing
remux/transcode is its own session). Named open items — do not treat as
passed:

1. Jellyfin `video.seekable.end(0)` on a remux session vs item runtime
   (teardown-2 full-title seekable claim).
2. Same-file far-seek: Nightjar vs Jellyfin, same box/offset, n≥5 each;
   confirm Jellyfin is remuxing/transcoding that file, not direct-playing.

**T5 condition 4 remains unscored.**

### Decision procedure

1. Run Step 1. If `MPV_V0` or `VLC_V0` ≥ 90% direct play → provisional engine
   win on *compatibility* share; **stop and report** before the Infuse/Jellyfin
   evening. Steps 2–3 of the original bake-off become confirmation/cost.
2. Measure bandwidth shape (1b) and transcode-cut predictability (1c) before
   the profile ADR.
3. Final decision names the path, the thresholds that fired, cost, what we
   give up, and Phase 4 / ADR-0020 consequences.

### Threshold-change log

| Date | Threshold | Change | Why |
|---|---|---|---|
| 2026-07-31 | T2 engine justify: warm far median ≤ **0.75 s** | Re-score: gate **not met** at CLI libmpv **1.05 s** / Flutter media_kit **0.91 s**; **met** at AetherEngine SPM probe **0.42 s**. Does **not** overturn the engine path vs copy HLS **4.62 s**. Formal amend candidate: raise warm justify to **≤ 1.25 s**, or keep 0.75 s as aspirational and treat “decisive vs deleted path” as the architecture rule. | 0.75 s was set with **no comparison baseline**. 1.05 s (and 0.91 s binding) against the path being deleted is still decisive for leaving AVPlayer remux. Aether clearing 0.75 s is a ranking input, not a reason to revive the deleted path. |

Previously the log read “(none)” while the decision proceeded past a failed gate — that made Step 0 decorative. This entry restores the discipline.

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

## Decision (current) — **PROVISIONAL**

**Still provisional.** What would make it final:

1. **Apple TV dogfood** of Dolby Vision display switching, HDMI audio
   passthrough, and Match Content — Aether vs native control on a real AVR
   chain. **media_kit is not a third leg on tvOS** (see Finding — media_kit
   / tvOS below). Closing Apple TV may flip the *tvOS* engine without
   overturning the macOS/iOS provisional pick.
2. **Full stratified T4 through bindings** (n=228), not the hard-codec head
   of the sample (see T4 note below).
3. **Contiguous full-title VOD** far-seek on a produced session (1c wire),
   not only “window moved via `POST …/seek?startMs=`”.
4. **Step 2 profile ADR** shipped enough that `/stream` honours a
   client-reported profile (end-to-end DP on the real server).

Until those close, Apple stays on **media_kit / libmpv** as the provisional
pick **for macOS and iOS**. That pick is **unproven on Apple TV / tvOS**,
which is the platform the Part C display/audio axes exist to test. Android
stays **Media3**. Sessions stay for bandwidth.

**Provisional reason (updated):** binding T2/T4 for media_kit now exist;
AetherEngine (Engine C) builds and measures outside Moonfin. The Apple choice
is still not final because TV display/audio axes and contiguous full-title
publish are open, Aether’s warm-far latency **clears** the 0.75 s gate while
media_kit does not, and **media_kit has no tvOS plugin** (stock Flutter or
[fluttertv/flutter-tvos](https://github.com/fluttertv/flutter-tvos)).

**Path we take (provisional)**

Ship **macOS / iOS** clients on **media_kit / libmpv** (Matroska direct play).
Treat **tvOS** as a separate engine decision: AetherEngine (or another
AVPlayer-presenting path) until proven otherwise. Keep **Media3 on Android**.
Keep sessions for bandwidth. Do **not** build product playback on stock
AVPlayer for the household library on Mac/iPhone. Do **not** choose
libVLC/VLCKit on current evidence.

**What it costs**

- Flutter shell + OSD/scrubber + media_kit on macOS/iOS. Upstream media_kit is
  dependency weight unless forked (Moonfin already forked `media_kit_video`
  once on Android — that is the maintenance class).
- AetherEngine alternative: ~48k Swift lines, Apple-only, single maintainer,
  prebuilt SPM binaries, LGPL-3.0 + App Store/DRM exception. If unmaintained
  we own a fork of that surface (realistic legally; expensive in practice).
  On macOS/iOS, media_kit remains the cheaper fork story; **on tvOS media_kit
  is not available without a Nightjar-owned port that would breach T3**.
- Sessions remain for remote/bitrate. Restart / full-title publish matter on
  that path (Part 3).

### Three-engine scoreboard (binding / SPM)

Instrument: `apps/engine-bakeoff/`. Raw: `notes/client-arch/bakeoff-runs/`.

**Pod / build (T3 evidence):** first `pod install` failed because
`flutter precache --macos` had never been run (`FlutterMacOS.xcframework`
missing). After precache, media_kit macOS builds. App Sandbox also blocked
reading the sample JSON until entitlements were disabled for the harness.

**URL resolution:** Nightjar `/stream` is `BROWSER_V0`-gated (415 on MKV).
Part A used `dp_byte_serve.py` on `:18097`. **Step 2 profile ADR is a client
prerequisite, not a follow-up:** a profile the client can report, and
`/stream` honouring it, before any engine is usable against the real server.

| Gate | media_kit (Flutter) | libVLC | AetherEngine (SPM probe) |
|---|---|---|---|
| **T2 warm far p50** | **908 ms** (n=11) | CLI seek figures **deleted** (log heuristics). Binding seek not scored. | **422 ms** (n=12) |
| **T2 cold far p50** | **804 ms** (n=10) | — | **358 ms** (n=12) |
| **T2 warm startup p50** | 670 ms | — | 753 ms |
| **T2 cold startup p50** | 2671 ms | — | 1691 ms |
| **T3** | Maintained plugin; precache tax. Bake-off Dart ~0.9k + tools. | No maintained macOS Flutter plugin — FFI (~0.3k) + VLC.app. | Builds outside Moonfin. Own ~48k Swift if forked; Apple-only. |
| **T4** | CLI full sample **0.44%** (n=228). Binding head n=40 **7.5%** (3× mpeg4 AVI) — **biased head, not library rate**; do not disqualify on this slice alone. | CLI **0%** (n=228). | SPM head n=40 **2.5%** (1× vc1 timeout) with DP up. Smoke on hevc/h264/DTS/TrueHD OK. |
| **T2 gate 1** (copy HLS) | **4621 ms** — fires; path deleted under engine DP. | same | same |

CLI libmpv warm far p50 was **1051 ms** — binding **908 ms** is the same band;
CLI figures stay as corroboration, not the scoreboard row.

**VLC CLI seek numbers remain deleted.** Do not cite heuristic VLC land times.

**Preference (ranked, provisional)**

1. **Flutter / T3 / forkability (macOS/iOS):** media_kit wins over libVLC (no
   plugin) and over Aether’s ownership tax. **tvOS:** media_kit has no `tvos:`
   plugin even under flutter-tvos; that axis does not apply without a T3-scale
   port.
2. **HTTP attach:** libmpv ranged (p50 1 MiB). VLC open-ended Range can pull
   multi-GB. Aether uses its own reader over HTTP (works on dp_byte_serve).
3. **T2 latency:** Aether clears ≤0.75 s; media_kit does not (0.91 s). Still
   ~5× better than copy 4.62 s. See threshold log.
4. **T4:** CLI media_kit/VLC clear 2%. Binding/Aether head slices are
   hard-codec oversamples — re-run full n=228 before flipping.
5. **Adaptive HLS:** tied (Step 3b); server-rung ABR not blocked.
6. **Apple display/audio (unmeasured on TV):** only Aether presents through
   AVPlayer — and media_kit does not run on stock tvOS (next finding).

### Finding — media_kit cannot target tvOS without a Nightjar-owned port

Investigation only (2026-07-31). Flutter **3.44.8** stable; resolved
`media_kit_video` **2.0.1**, `media_kit_libs_ios_video` **1.1.4**,
`media_kit_libs_macos_video` **1.1.4** from the pub cache (not pub.dev prose).

**Known tvOS Flutter path:** [fluttertv/flutter-tvos](https://github.com/fluttertv/flutter-tvos)
(currently tracks the same Flutter SDK rev as this bake-off’s `.tools/flutter`).
It is a drop-in CLI + custom tvOS engine; plugins must declare an explicit
`flutter.plugin.platforms.tvos` key — packages with only `ios:` are **not**
loaded. Curated ports live under [fluttertv/plugins](https://github.com/fluttertv/plugins)
/ pub.dev `fluttertv.dev` (`video_player_tvos`, `audioplayers_tvos`, etc.).
**`media_kit` / `media_kit_video` are not in that index.**

| Evidence | Result |
|---|---|
| `media_kit_video` pubspec `plugin.platforms` | android / ios / macos / windows / linux / web — **no `tvos:`** |
| `media_kit_video` iOS podspec | `s.platform = :ios, '9.0'` |
| `media_kit_video` iOS `Package.swift` (upstream main) | `platforms: [.iOS("9.0")]` only |
| `media_kit_libs_ios_video` podspec | `s.platform = :ios, '9.0'`; Makefile downloads **ios-universal** libmpv xcframework only |
| `media_kit_libs_macos_video` podspec | `s.platform = :osx, '10.9'` |
| Upstream `media-kit/media-kit` | No open/closed issue that adds tvOS as a supported platform. README platform table omits tvOS. |
| `media-kit/libmpv-darwin-build` #43 | Open PR adding tvOS/tvOS Simulator libmpv build targets. Maintainer (**birros**, 2026-04-20): Flutter does not officially support Apple TV, so **tvOS is not an immediate priority** for the package that consumes this repo; willing to consider non-interfering flags later. Contributor published fork artifacts; **not** merged into the Flutter plugins. |
| Stock Flutter harness | `flutter create --platforms=tvos .` in `apps/engine-bakeoff` → **`"tvos" is not an allowed value for option "--platforms"`**. First hard failure on the **stock** tool. Stopped; no fix. |
| flutter-tvos | Would clear that tool failure. Still would not pick up media_kit until a federated `*_tvos` plugin (and tvOS libmpv slice) exists — neither upstream nor in fluttertv’s curated list. |

**Port cost (if forced on flutter-tvos):** Nightjar would own (a) adopting
flutter-tvos as the Apple TV Flutter toolchain (engine/CLI dependency outside
stock Flutter), (b) a `media_kit_libs_*_tvos` rebuild pipeline (Nix/meson
darwin-build + xcframework, including the unmerged #43 work), and (c) a
`media_kit_video` tvOS federated plugin (`tvos:` key + texture/Metal +
registrar; `flutter-tvos plugin port` scaffolds, Nightjar finishes and keeps).
That is well above the T3 gate (**> 2 500 Nightjar-owned lines per platform**,
tvOS counted separately) before product glue — not a thin binding.

**Consequence for Part C:** “media_kit vs Aether vs native on Apple TV”
assumed media_kit runs on tvOS. On stock Flutter it does not even build; on
flutter-tvos it still needs an owned plugin+libs port that is not done and
would breach T3. Apple TV Part C is **Aether vs native** (and any future
tvOS-capable engine already under the line), not a three-way with stock
media_kit.

### Finding — session far seek (surviving path) — cause confirmed

Raw: `notes/client-arch/bakeoff-runs/part3-session-seek.json` (item 3997).

| Observation | Result |
|---|---|
| Live playlist type | **EVENT** (windowed) |
| Initial sum(EXTINF) vs title | **8 s** vs **2584 s** |
| Far seek on window at start | **fail** (timeout) |
| After `POST /sessions/{id}/seek?startMs=` at 75% | New `run_N`; far seek on that window **lands** (~2.2 s) |
| Discontinuous multi-run VOD mirror (sum ~240 s) | **fail** — not contiguous full-title |
| Contiguous full-title VOD 0…far | **Not produced** this pass (full cook or short title still open) |

**Cause (from request/playlist shape, not assumption):** the engine can only
seek inside the produced window. Same class of failure as AVPlayer on
windowed sessions. Moving the window via `startMs` fixes land for that
offset; it is not the 1c product contract.

**Named fix for the profile ADR:** publish honest **full-title VOD** for
transcode sessions (Step 1c; 0/192 mid-start mismatches). Contiguous body
still needs one clean measure before calling 1c closed on the wire.

**Throttle:** configured **2.72 Mbps**, achieved **~1.90 Mbps** avg
(`partb-starve.json`). Report **achieved** as the starvation figure.

**ABR signals:** media_kit + libVLC expose stall/buffer;
`stop_gate_neither_signal` = false.

### Part C — eyes-on (macOS limits)

Capability tables are vendor claims until watched. This pass:

| Axis | Status |
|---|---|
| HDR vs native | **Open on macOS eyes-on.** Aether opens 2160p HEVC (id 545) via SPM probe; ranking-changing HDR delta **not** scored by looking this session. |
| Multichannel | Aether smoke: DTS 5.1 (id 32) and TrueHD 7.1 (id 35) reach first frame. Passthrough vs downmix **not** verified on an AVR. |
| Subtitles | Household PGS titles exist (e.g. id 34, 43). ASS+PGS styling/position **not** eyes-on scored. |
| Composition | media_kit `Video` composites under Flutter widgets in `engine_bakeoff`. Aether Flutter path is Moonfin-shaped platform view (`moonfin/aether_video`) — prior art only; not copied. Glue cost = AppKitView + channel, unmeasured in Nightjar tree. |

**macOS cannot answer (named open items for Apple TV + real AV chain):**

1. Dolby Vision display switching
2. HDMI audio passthrough (E-AC-3 JOC / TrueHD / DTS)
3. Match Content

These are why AetherEngine remains a candidate on **tvOS**. Do not conclude
them from a laptop. Do not schedule media_kit as a Part C peer on Apple TV.

### AetherEngine dependency assessment

| | |
|---|---|
| Builds outside Moonfin? | **Yes** — stock SPM `Package.swift` + probe binary. |
| Licence | **LGPL-3.0 + Apple Store/DRM exception** (Vincent Herbst). FFmpegBuild / LibDovi remain separate LGPL pieces as in the teardown. |
| Shape | Prebuilt SPM binary deps, ~48k Swift lines in checkout, **Apple-only**, single maintainer. |
| If unmaintained | We own a fork of that Swift surface + binary rebuild pipeline. Legally fine under LGPL+exception; practically a product team’s media engine. |
| vs media_kit | **Platform-conditional.** On **macOS/iOS**, media_kit is cross-platform, forkable (Moonfin already forked `media_kit_video` on Android), and cheaper long-term ownership. On **tvOS**, stock Flutter rejects the platform; [fluttertv/flutter-tvos](https://github.com/fluttertv/flutter-tvos) is the known toolchain, but media_kit still has no `tvos:` plugin (and is absent from fluttertv’s curated ports). A Nightjar port would own flutter-tvos adoption + libs rebuild + federated plugin and would breach T3. There Aether’s Apple-only cost is compared to “no media_kit,” not to a cheap fork. |

### What blocks the engine from being usable (Part 5)

Direct play against Nightjar `/stream` is **unproven end to end** while the
route is `BROWSER_V0`-gated. Bake-off DP used a local byte server. **Step 2’s
profile ADR is a prerequisite for the client**, not a follow-up. Minimum:

1. A `ClientCapabilityProfile` the Flutter client can report.
2. `/stream` (and playback-info) honouring that profile for Matroska DP.

**Server consequence**

- Sessions stay for bandwidth (+ burn-in / web / outliers).
- Full-title publish (1c) is the scrub contract on that path; windowed EVENT
  + `startMs` is today’s behaviour and explains the far-seek finding.
- ADR-0020 stands for copy/remux.

### Open checklist

1. Jellyfin seekable comparison (T5-4) — unscored.
2. Step 2 profile ADR + N100 capacity.
3. Contiguous full-title VOD far-seek measure.
4. Full n=228 binding T4 for media_kit and Aether.
5. Part C eyes-on on macOS (HDR/subs).
6. **Apple TV Part C:** DV / HDMI / Match Content as **Aether vs native**
   (media_kit has no stock or fluttertv-curated tvOS plugin — finding above).
   Decide tvOS engine separately from the macOS/iOS media_kit provisional
   pick. flutter-tvos is assumed as the Flutter shell path if we ship a
   Flutter Apple TV app at all.
7. libVLC binding seek land (or leave libVLC out of Apple).

**T5 condition 4 remains unscored.**

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

### Step 3 — ABR under engine (recorded)

Instrument: `scripts/abr_throttle_probe/` (three-rung ladder + 1 Mbps byte-rate
proxy). Same playlist, same throttle. Source: Elementary 3x05 mid-title 60 s
window; rungs hi/mid/lo at ~4.5 / 1.7 / 0.75 Mbps tagged BANDWIDTH.

Step 3b is narrow: both engines cleared T1 at 100%, so if we put an engine
on Apple, is libmpv or libVLC better on adaptive HLS. Not an Android question
(Media3 already clears 96.5% and adapts). Desktop VLC 3.0.23 stands in for
libVLC (same engine and adaptive demuxer; VLCKit is only the Apple binding).
Master URL needed `:demux=adaptive` or VLC mis-demuxes the ladder. Options
checked against this build: `--adaptive-logic
{,predictive,nearoptimal,rate,fixedrate,lowest,highest}`.

| Client | Under 1 Mbps throttle |
|---|---|
| **mpv 0.41** (`--hls-bitrate=max`, the default) | Probes early segments of all three rungs, then **stays on hi**. hi segments take **8–12 s** to fetch for 2 s of media. No mid-stream downshift in 30 s. mpv's only HLS bitrate choices are `no` / `min` / `max` / integer — **no auto ABR mode**. |
| **hls.js** (web's bundled build, Chrome CDP) | Starts on hi (`startLevel: 2`), **downshifts to lo within ~1.1 s** of the first level event (t≈8.8s → 9.9s). Proxy sequence `hllllllll…`. One non-fatal `bufferStalledError`, then sustained play on lo (~1.3–1.7 s/seg). |
| **libVLC** via desktop VLC 3.0.23. Default `--adaptive-logic` (empty) and `--adaptive-logic=nearoptimal` | Starts on **lo** (seg0 ~1.4 s), **upshifts to hi** at ~1.4 s, then **stays on hi** (`lhhh…`). hi segments **~8.5–9 s** for 2 s of media. No mid-stream downshift in 40 s. PCR-late / rebuffering while starved. |
| **libVLC** `--adaptive-logic=rate` (also `lowest` sanity) | Starts on **lo** and **never leaves** (`llllllll…`). Segments ~1.2–2.0 s for 2 s media; no PCR-late stalls. Not a downshift: it never climbs. |

Raw: `notes/client-arch/abr-mpv-max-log.json`,
`notes/client-arch/abr-hlsjs-events.json`,
`notes/client-arch/abr-hlsjs-access.json`,
`notes/client-arch/abr-vlc-default-log.json`,
`notes/client-arch/abr-vlc-nearoptimal-log.json`,
`notes/client-arch/abr-vlc-rate-log.json`,
`notes/client-arch/abr-vlc-lowest-log.json`.

**What the user sees:** libmpv on a multi-variant playlist under a thin link:
stalls / waits on the top rung. Default / nearoptimal libVLC: brief lo start,
then the same starvation stall after it climbs. `rate` / `lowest`: continuous
play on lo, no switch artefact. hls.js: brief stall, then continuous play at
the sustainable rung after a real downshift.

**Interpretation (Step 3b only)**

1. **libmpv vs libVLC:** neither mid-stream downshifts under starvation at
   defaults. libVLC climbs into the same trap; `--adaptive-logic=rate` (or
   `lowest`) avoids the climb but is not hls.js-class ABR. On adaptive HLS
   alone they are **tied**. That is one axis. Flutter integration maturity,
   subtitle rendering, and platform audio handling are probably bigger inputs
   to the Apple-engine choice and are all unmeasured.
2. **ABR is not blocked either way.** Rendition selection can move
   server-side: the client reports throughput and gets a new session at a
   different rung, reusing the existing restart machinery. Slower than
   hls.js at ~1.1 s, but it works with a client that cannot adapt. Do not
   read the mpv (or libVLC default) row as "ABR is impossible."
3. **v1 is unaffected.** ADR-0008 fixes v1 at one server-chosen rendition
   (Auto / High / Original). This row is post-v1 input only.

### Parked for Phase 4 (do not resume under Phase 2 / Gate 2)

Explicit park (2026-08-01). Gate 2 and Phase 2 entry close first.

- Remaining bake-off work: AetherEngine ranking, Part C on Apple TV hardware,
  full n=228 binding T4, Jellyfin T5-4, contiguous full-title VOD wire measure.
- ADR-0021 (client architecture) stays **proposed**. Apple engine stays
  unresolved. Do not finalise it while Gate 2 is open.
- Keyframe-map feasibility note stays a note until Phase 4 opens a schema ADR.
- **Constitution conflict (deliberate):** `ENGINEERING_RULES.md` Rule 2.4 still
  says one shared Rust/libmpv player core. ADR-0021 contradicts that for the
  client stack. An ADR does not amend the constitution. Resolve the conflict
  when Phase 4 opens (amend Rule 2.4 with unanimous approval, or revise the
  ADR). Catch it on purpose then; do not discover it as a surprise mid-client.

### ADR-0020 (Phase 2 / sessions — not parked)

- ADR-0020 (producer-owned time-keyed segments) is the copy/remux session fix;
  merge to `main` before trusting household playback or reusing last week's
  seek numbers against trunk. Transcode still needs honest full-title publish
  (1c); Part 3 confirmed EVENT windows block far seek until `startMs` moves
  the window.
- Step 2 profile ADR + N100 capacity remain Phase 2 Gate 2 work (not Phase 4).
