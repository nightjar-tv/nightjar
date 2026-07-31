# ADR-0021: Client architecture (Flutter UI, per-platform engines)

- Status: proposed
- Date: 2026-07-31
- Supersedes: ADR-0001 (pending stub); Rule 2.4's "one shared Rust/libmpv
  core" wording for the client player stack (constitution table §1 / Rule 2.4)
- Evidence: `notes/client-arch/engine-bakeoff.md`

## Context

The constitution locked Flutter over a single Rust/libmpv player core
(Rule 2.4). That wording assumed one engine would clear every household
platform. The engine bake-off measured otherwise: Android Media3 already
direct-plays 96.5% of the dogfood library without an engine; Apple AVPlayer
is at 12.9% because Matroska is the library default; Tizen, webOS, and the
web client cannot direct-play and live on sessions; media_kit has no tvOS
plugin even under flutter-tvos. Media3-on-Android already established
per-platform engines as an accepted class. This ADR populates that class
rather than inventing a new exception type.

ADR-0001 left platforms pending. This record closes what the bake-off can
close and leaves the Apple engine choice unresolved on purpose.

## Decision

Flutter for the client UI on every platform with a working toolchain, with a
per-platform playback engine behind one Dart player interface.

| Platform | UI toolchain | Engine | Playback path | Status |
|---|---|---|---|---|
| macOS, iOS | Flutter (stock) | **Unresolved** ((a) or (b) below) | Direct play when the profile allows; sessions for bandwidth / burn-in / outliers | Engine open; UI decided |
| tvOS | Flutter only if flutter-tvos stays healthy; else native SwiftUI | Depends on Apple choice (a)/(b); not stock AVPlayer as the preferred engine | Direct play and/or session per engine | Open (shell tied to fork) |
| Android, Android TV | Flutter (stock) | Media3 | Direct play (96.5% on dogfood) | Decided |
| Windows, Linux | Flutter (stock) | libmpv via media_kit | Direct play | Decided |
| Tizen (6.0+) | Flutter (flutter-tizen) | Platform / vendor player | Session | Decided (engine = vendor) |
| webOS (26+) | Flutter (flutter-webos) | Platform / vendor player | Session | Decided (engine = vendor) |
| Web | SvelteKit | hls.js (desktop Safari per ADR-0017; iOS Safari native-hls) | Session | Decided |
| Vega OS | n/a | n/a | n/a | Out |
| PlayStation, Switch | n/a | n/a | n/a | Out |

### Apple engine (unresolved)

Do not inherit a provisional pick. Two options remain, gated on bake-off
open items (Part C on Apple TV hardware, Aether full n=228 T4, measured
Flutter platform-view glue cost in the Nightjar tree):

**(a) AetherEngine on macOS, iOS, and tvOS.** One engine and one capability
profile across every Apple device. It presents through AVPlayer, so AirPlay,
PiP, Now Playing / lock-screen controls, background audio, and Dolby Vision
display switching are inherited rather than owned. Cost: Apple-only, ~48k
Swift lines, single maintainer, LGPL-3.0 with App Store / DRM exception
(FFmpegBuild / LibDovi remain separate LGPL pieces).

**(b) media_kit on macOS and iOS; tvOS still needs a Matroska-capable engine.**
Reuses the libmpv glue Windows and Linux require anyway. On phone and desktop
that is the cheaper long-term fork story. On tvOS, upstream media_kit has no
`tvos:` plugin (stock Flutter or flutter-tvos curated ports), so (b) is not
"media_kit everywhere on Apple." The living-room leg is still open between
Aether (or another AVPlayer-presenting engine that clears Matroska) and a
Nightjar-owned media_kit tvOS port (flutter-tvos adoption + libmpv slice +
federated plugin), which the bake-off scores as a T3 breach.

**Stock AVPlayer on tvOS is not a peer alternative under (b).** T1 on
`APPLE_AVPLAYER_V0` is 12.9% direct play and ~80% container remux: the status
quo the bake-off argued against (producer-truth scrub ceiling on that path,
Atmos only where AVPlayer accepts it, and the rest of the catalogue from the
spike). Those numbers disqualify stock AVPlayer as the preferred tvOS engine.
They do not disqualify (b) as a whole, because (b)'s phone/desktop leg is
media_kit; they only mean tvOS under (b) must still pick a Matroska-capable
engine, not fall back to remux-as-default.

Committing to Windows and Linux means libmpv glue is owned regardless. Option
(a) does not reduce the product to one engine family.

**Fallback if Aether goes unmaintained (written now, calm):** stock AVPlayer
plus remux/transcode sessions. That is the same 12.9% / 4621 ms profile
above. Acceptable as a retreat on any Apple surface that loses its Matroska
engine, not as the preferred path and not as the hidden meaning of (b).

### Sessions are first-class

Sessions are a permanent product path, not leftover remux architecture.
Tizen, webOS, web, and any cast path can never direct-play the household
Matroska library. Contiguous full-title VOD publish for transcode sessions
(Step 1c; mid-start cut mismatches 0/192) is therefore a **shipping
requirement**, not cleanup on a path being deleted.

The bake-off line "the path this measured is being deleted" is correct for
Apple and desktop engines that Matroska-direct-play (copy/remux HLS stops
being the common LAN path). It is **wrong for TVs and the web**, where the
session path remains the product. Do not cite the 4.62 s copy far-seek as
"sessions are unacceptable" after engines land on phone and desktop.

### Toolchain forks are the Rule 4.4 liability

Engines are chosen per platform. The liability that needs explicit acceptance
is the **UI toolchain fork**, not the player binary:

- **flutter-tvos and the tvOS shell are one item.** Stock Flutter rejects
  `tvos` as a platform. A Flutter client on Apple TV exists only while
  [fluttertv/flutter-tvos](https://github.com/fluttertv/flutter-tvos) (community
  fork of the Flutter engine and CLI) stays healthy enough to ship against.
  If that fork stalls, tvOS is not a Flutter client regardless of engine
  choice; the shell falls to native SwiftUI. Do not resolve "Flutter vs
  SwiftUI on tvOS" without checking the fork's health: they are the same
  open question.
- **flutter-tizen:** vendor-maintained Samsung extension
  ([Samsung Flutter for Tizen](https://developer.samsung.com/smarttv/develop/native/flutter.html)).
  TV products moved the toolchain GCC 9.2 to 14.2 for 2026 models; apps and
  prebuilt plugin libraries must ship ABI-compatible rebuilds. That ABI churn
  is why no Nightjar engine goes on a Samsung TV: the vendor player rides the
  vendor rebuild.
- **flutter-webos:** vendor-maintained LG extension (webOS 26+;
  [Flutter for webOS](https://webostv.developer.lge.com/develop/guides/flutter-for-webos)).
  Host builds are Ubuntu-only per LG's guide.

### Profiles the Step 2 ADR must define

This ADR determines which profiles exist; the Step 2 profile ADR defines
bitrate / resolution / HDR fields and wire shape:

| Profile | Role |
|---|---|
| `MEDIA3_V0` | Android / Android TV (scored; 96.5% DP) |
| `MPV_V0` | Windows / Linux media_kit; also the bake-off floor for libmpv (100% DP on dogfood) |
| `AETHER_V0` | Apple if (a) wins (**unscored**; needs a `t1_profile_counts.py` run before Gate 1 language is reused) |
| `BROWSER_V0` | Web (and today's `/stream` gate until clients report a real profile) |
| Tizen / webOS model-year profiles | Vendor player capabilities vary by TV year; client-reported profiles are required, because the server cannot guess the stick |

Per-model-year capability is why profiles are client-reported, not
server-inferred from UA alone.

### Dart player interface

One Dart interface (attach, seek, track selection, state events, error
taxonomy) sits over Media3, media_kit/libmpv, Aether or AVPlayer, hls.js /
native HLS, and vendor TV players. That is an earned abstraction under
Rule 4.7: four or more concrete implementations, not a speculative trait.
It is also what keeps T3 under budget: OSD, scrubber, and dusk-strip seek
are written once and talk to the interface.

**Boundary rule on the interface:** the client reports capability as data
(a profile id plus fields the Step 2 ADR defines); the server decides the
playback method (direct play, remux, or transcode session). Clients do not
choose the method locally from heuristics. That constraint is what keeps
Jellyfin's client-side decision failure mode out of the product, and it
belongs on the interface contract rather than as an assumed habit.

### Out of scope platforms

**Vega OS.** Linux-based, not AOSP; no APK sideload for consumers; React
Native for Vega or web (WebView) only; Appstore distribution. A cloud-APK
bridge exists only for Amazon-selected titles. The Android Flutter build
covers current Fire OS / Fire TV hardware and decays as sticks turn over to
Vega. See [Amazon Vega developer docs](https://developer.amazon.com/).

**Xbox.** Covered by the existing web client (no separate native shell).

**PlayStation, Switch.** No public app route that fits the Flutter + API
model; out.

## Alternatives considered

**One Rust/libmpv core on every platform (constitution as written).** Would
satisfy Rule 2.4 literally and keep one bug surface. Rejected: Media3 already
clears Android without it; stock Flutter has no tvOS target and media_kit has
no `tvos:` plugin; Tizen/webOS ABI and vendor-player policy make a Nightjar
engine on those TVs a Rule 4.4 and T3 mistake. Keeping the wording would
force fiction on platforms we will ship.

**AVPlayer everywhere on Apple, sessions for Matroska.** Matches today's
web/safari habits and avoids Aether ownership. Rejected as the preferred
path: 12.9% direct play and 4621 ms copy far-seek are the measured cost of
that retreat. Kept only as the Aether-unmaintained fallback.

**Native Swift/Kotlin app shells with no Flutter.** Avoids flutter-tvos and
vendor Flutter extensions. Rejected for phone/tablet/Android TV: one UI
codebase and one OSD implementation are the T3 budget. On tvOS, native
SwiftUI is the fallback when flutter-tvos stalls (same open item as the
fork, above), not a parallel shell decision.

**Engine on Tizen/webOS (media_kit or Aether).** Would raise direct-play share
on LAN to those living rooms. Rejected: vendor GCC/NDK churn (Samsung 2026
GCC 14.2) and the absence of a maintained tvOS-class media_kit port make the
Nightjar-owned lines the product. Platform player + session is the honest
path.

**Web as the only TV client (no flutter-tizen / flutter-webos).** Avoids
vendor SDK forks. Rejected for 10-foot UX and store distribution on those
platforms; the web client remains the Xbox and browser path.

## Consequences

**Good**

- Rule 2.4's intent (fix playback once per bug class) moves to the Dart
  interface and server session contract, not a single native binary.
- Android ships on evidence already in hand.
- Windows/Linux share media_kit with a clear profile (`MPV_V0`).
- Apple can still choose unity (a) or media_kit phone/desktop with a separate
  Matroska-capable tvOS engine (b) after hardware Part C, without treating
  stock AVPlayer remux as the TV answer.
- Sessions and full-title VOD are funded as product work for every
  non-direct-play client.

**Bad (accepted)**

- Two engine families to debug at steady state (Media3 + libmpv, and possibly
  Aether on Apple) instead of one Rust core. Under (b), Apple may itself be
  two engines (media_kit + a tvOS Matroska engine).
- If flutter-tvos stalls, two UI shells on Apple (Flutter phone/desktop,
  SwiftUI TV): that is the fork liability materialising, not a second open
  question.
- Forked Flutter SDKs on two TV platforms (flutter-tizen, flutter-webos), plus
  flutter-tvos while Apple TV stays Flutter: Rule 4.4 liabilities we keep.
- TV playback quality is permanently bounded by the vendor's player and
  model-year profile; Nightjar cannot patch that from the app.
- `/stream` remains unusable for real direct play until the Step 2 profile
  ADR lands and clients report a profile (today's `BROWSER_V0` gate).

**Does not close**

Status stays **proposed**. Bake-off open items (Apple Part C, Aether n=228 T4,
contiguous full-title VOD wire measure, Jellyfin T5-4) and this ADR's final
acceptance are **parked for Phase 4**. Step 2 profile ADR + N100 capacity
remain Phase 2 / Gate 2. No T-gate threshold from the bake-off is moved here.

**Constitution conflict (not resolved by this ADR):** ENGINEERING_RULES.md
Rule 2.4 still requires one shared Rust/libmpv player core. This ADR
contradicts that wording for the client stack. Resolve when Phase 4 opens —
amend the constitution (Rule 6 / locked-doc process) or revise this record.
Do not treat "accepted ADR" as a silent rewrite of Rule 2.4.
