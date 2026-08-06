# ADR-0016: Reject playlist-as-sole-authority seek rewrite

- Status: accepted
- Date: 2026-07-28

## Status

The decision is **accepted**: the playlist-as-sole-authority rewrite is
**rejected**. Kept as a record so the proposal is not revived without new
evidence. (`Status: accepted` above refers to this ADR's own decision, which was
to reject the proposal it describes. `rejected` on an ADR means the ADR's own
proposal lost.)

## Context

After a night of native Safari scrub-before-land failures, a draft proposed
making the HLS playlist the sole authority for segment targeting: collapse
session land fields, long-poll playlists until "accurate," list only
servable segments, and delete client `segmentIndexAtSeconds`-driven
fetches. That draft is not an accepted ADR and must not be implemented.

Separately, branch `experiment/playlist-only-seek` (2026-07-27) forced
`decide_segment_miss` to always `Wait` so only playlist `?startMs=` could
restart FFmpeg. Safari cold scrub scored **BROKEN** (no restart / no useful
`start_ms` traffic). That experiment already closed "playlist-only seek"
as a permanent architecture.

## Decision

Reject the playlist-as-sole-authority rewrite.

Reasons (from code and experiments, not from the draft's framing):

1. **ADR-0011 amendment** already requires full-title VOD listing and
   load-bearing 503s. "Only list servable segments" reverses that and
   reintroduces the mid-window scrubber-clock failure.
2. **Playlist-only seek** (segment-miss never Restart) failed dogfood.
3. Native A/V segment GETs are owned by **WebKit** from `video.src`, not by
   JS parsing an M3U. A client "follow the playlist list" rewrite does not
   describe the native media path.
4. The scrub-before-land race traced in current `asset_wait` is a
   **stale-serve** window: read segment bytes → apply pending restart
   (moves `play_start_ms`) → still `Ok(bytes)` for the prior land. That is
   fixed by re-checking `play_start_ms` before returning, not by a new
   authority model.

## Consequences

- Keep coalesce fields (`pending_play_ms`, `play_start_ms`, `start_ms`,
  `first_segment_ready`) and segment-miss Restart guards (ADR-0011 §8).
- Do not open a "servable-only playlist" or "playlist sole seek authority"
  slice without new measured evidence that overturns (1)–(4).
- Routine stale-read fixes do not need a further ADR when they do not
  change the session model.
