# ADR-0017: Desktop Safari attaches with hls.js

- Status: accepted
- Date: 2026-07-29

## Context

ADR-0007 §6 chose native HLS on any browser where
`canPlayType('application/vnd.apple.mpegurl')` is non-empty, and hls.js
elsewhere. In practice `pickBackend` treats all Apple WebKit HLS engines
alike: desktop Safari and iOS/iPadOS both take `native-hls`. That matched
Gate 2 and kept one Apple path.

Phase 2 Safari scrub dogfood (12 Angry Men Bluray-1080p, VideoToolbox,
`NIGHTJAR_DISABLE_PREEMPT=1`, encode lead 2, same release binary) measured
post–land-ensure quiet separately from cook time. Land-ensure returns 200
when the land `.m4s` is on disk; quietGap is then the delay until the first
non-probe media GET starts.

Same-build A/B via opt-in `?njHlsJs=1` (probe only; default pick unchanged):

| AttachBackend | quietGapMs (five singles) |
|---|---|
| `native-hls` (desktop Safari) | 1681, 0, 1262, 2923, 639 |
| `hls-js` (forced on same Safari) | 0, 1, 0, 1, 0 |

Native trials show empty buffer and `seeking=true` for hundreds to thousands
of milliseconds after nudge while only playlist GETs fire; the land segment
GET often starts ~0.6–3 s later even though land-ensure already proved the
bytes servable. hls.js starts the first media fetch at nudge ±1 ms every
time. Recover-to-advance stays ~1.5 s on hls.js; native recover spreads
~1.5–4.5 s when quietGap is large.

Connection contention (land-ensure still open at nudge) was falsified earlier
on the same instrumentation: success trials had `ensureOpenAtNudge=false`.
Encode cook dominates wall scrub time on both arms and is out of scope here.

Session correlation against published WebKit / hls.js scheduler notes
(seeking property vs queued seeking task; MSE readyState timer behaviour
while paused; hls.js ~100 ms load tick) does not predict the measured
quietGap spread on the native path (all recover-watch samples were
`paused=false`; dogfood was not on the hls.js tick). The A/B isolates the
variable: same server, same ensure→nudge contract, same title — only the
client fetch owner changes. Native AVFoundation HLS owns segment dispatch
after `video.src`; application JS cannot force that engine to issue the land
GET sooner. MSE + hls.js owns fragment loading and does not exhibit the gap.

ADR-0011’s rejection of “force hls.js on Safari to hide a lying playlist”
stands for that rationale. This decision is not that workaround: the
playlist is already full-title VOD with load-bearing 503s; the defect is
post-ready native fetch timing on desktop Safari.

Rule 2.4 and Rule 2.6 still bind: iOS/iPadOS remains on native HLS (required
there; hls.js is not the product path on those devices). This ADR changes
**AttachBackend selection** on desktop Apple WebKit only. It does not add a
second player stack, remount `video.src` for scrub, or diverge server
protocol by client.

## Decision

1. **Desktop Safari (and other non-iOS / non-iPadOS Apple WebKit that can
   play HLS) defaults to `hls-js` when MSE / hls.js is supported.**
2. **iPhone, iPad, and iPod stay `native-hls`** when
   `canPlayType('application/vnd.apple.mpegurl')` is non-empty.
3. **Chromium-family and other MSE browsers stay `hls-js`** (unchanged).
4. **Odd WebViews** that are not Apple mobile, lack MSE, but claim native
   HLS may still attach native (last-resort path already in `pickBackend`).
5. **Server session / playlist / segment contracts unchanged** (ADR-0007,
   ADR-0011). Client UA fork remains the only attach fork (ADR-0011).
6. **Amend ADR-0007 §6:** “Safari plays HLS natively” becomes “iOS/iPadOS
   Safari (and other iPhone/iPad/iPod WebKit) play HLS natively; desktop
   Safari uses hls.js when MSE is available.”
7. **Land-ensure / currentTime nudge** remains available on the native
   path for iOS. Desktop Safari on hls.js uses the existing hls.js scrub
   path (`startMs` + subtitle `startLoad`); do not carry the native-only
   quietGap probe harness into the product default.

## Consequences

- `isAppleWebKitHlsEngine()` today is engine-only by design. Implementation
  needs an explicit **platform** split (iPhone / iPad / iPod vs desktop
  Apple WebKit), including iPadOS versions that spoof a Macintosh UA.
  That is new UA surface and ongoing maintenance (treat like a dependency:
  justify the predicates, keep them narrow, test the matrix).
- **Subtitle gate: closed (2026-07-29).** Founder dogfood on the product
  hls.js path (desktop Safari + Chrome): mid-title scrub, continue playing,
  and further scrubs show captions with TextTrack times matching wire
  absolute (`applyAbsoluteCueTimesFromVtt` after subtitle `FRAG_LOADED`;
  ADR-0013 sticky-baseline note). Native cue-inject stays native-only
  (iOS/iPadOS).
- Desktop Safari loses the native HLS hardware path for this web product
  surface; iOS/tvOS native requirements are unchanged.
- Opt-in `?njNativeHls=1` forces native on desktop Apple WebKit for
  regression comparison. The former `?njHlsJs=1` probe is removed; hls.js
  is the desktop default.
- Forever-refuse/404 papering and desktop-only protocol forks remain
  rejected (Rule 2.6). Backend selection stays the approved *attach*
  lever; see the amendment below for the scoped native-land remount.

## Amendment (2026-07-29): post-land `#t=` on native HLS

### Context

Seek-restart rewrites `init.mp4` under the same `#EXT-X-MAP` URI. Native
WebKit keeps the attach-time init; after land-ensure 200, `currentTime`
nudge alone leaves buffered ranges at the pre-scrub head and
`seeking=true` (scrub-before-play dogfood, item 33). Remounting
`video.src` *as the scrub* (before land bytes exist) was rejected earlier:
it remounts into an unready encode window and can exit fullscreen on some
paths. That rejection stays.

Ablation under preempt-on (same harness, N=5): full stack **5/5** Safari
native + Chrome; removing only the post-land `#t=` reassignment → Safari
**0/5** stick, Chrome still **5/5**. No narrower init-rebind (distinct MAP
URI without remount) was feasible in that matrix. Post-land `#t=` is the
measured unlock for native scrub-before-play on this product path
(`?njNativeHls=1` / iOS native).

### Decision

1. **After land-ensure 200 on the native path**, re-assign the *same*
   session master URL with `#t=<land seconds>` under seek suppress so
   WebKit reloads init + land segments. Not a new session POST.
2. **Do not** use `#t=` / `video.src` remount as the scrub itself (before
   land ready). Scrub intent remains `startMs` + land-ensure; remount is
   only the post-ready init rebind.
3. Desktop product default remains hls.js (this ADR). The remount applies
   on the native path (iOS/iPadOS and `?njNativeHls=1` dogfood). hls.js
   already rebinds MSE; it does not need this remount for the same
   failure mode.
4. Encode lead must cover dig-back after this reload when
   `#EXT-X-START` uses `PRECISE=YES` (couple documented in ADR-0011;
   lead value amendment held until Safari-confirmed minimum).

### Consequences

- Prior “do not revive `video.src` remount” wording is narrowed: scrub-time
  remount stays dead; **post-land-ensure** remount on the same master URL
  is accepted for native init refresh.
- UI may briefly reset chrome after remount (founder note: does not drop
  fullscreen). Polish is separate from this contract.
- Distinct MAP URI without remount remains unproven; do not substitute it
  for this amendment without new evidence.
