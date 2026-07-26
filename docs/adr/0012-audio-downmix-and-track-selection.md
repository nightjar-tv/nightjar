# ADR-0012: Audio downmix and multi-track selection

- Status: accepted
- Date: 2026-07-26

## Context

Sessions map only the first audio stream (`-map 0:a:0?`, ADR-0006 /
ADR-0011). Multi-language MKVs and commentary tracks are unreachable.
Transcode already forces stereo AAC via `-ac 2`, but FFmpeg's default
swresample matrix often under-weights the centre channel, so dialogue
vanishes under music — the classic media-server audio complaint. Direct play
and remux copy pass multi-channel AAC through untouched; `BROWSER_V0` has no
channel ceiling, so a 7.1 AAC MP4 is declared DirectPlay even though browsers
cannot render it usefully.

ADR-0006 named this follow-up: inventory, stable `trackId`, session switch
model, downmix rules, capability profiles. ADR-0011 made remux and transcode
the same session surface so audio switching is solved once.

## Decision

1. **Capability-driven downmix.** Downmix when the selected track's channel
   count exceeds `ClientCapabilityProfile.max_audio_channels`.
   `BROWSER_V0` sets `max_audio_channels: Some(2)`. No user preference to force
   stereo in this slice (no settings UI yet). **Phase 3 addition point:** a
   "force stereo" preference is a real user want once users and settings exist;
   it is additive on top of this rule and does not change the default.

2. **Hybrid session when only layout forces work.** When codecs would otherwise
   allow copy (`SessionMode::Copy`) but the selected track exceeds the channel
   ceiling, the session keeps `-c:v copy` and encodes audio only
   (`-c:a aac` + pan to stereo). Re-encoding video because audio has too many
   channels is absurd. This is a distinct mode from full transcode, not a
   buried row in a behavior table. Full transcode still re-encodes video and
   applies the same pan when layout exceeds the ceiling.

3. **Explicit pan matrix; LFE included at low gain.** Do not rely on bare
   `-ac 2`. Use an FFmpeg `pan=stereo|...` filter with AT-style coefficients
   so centre is mixed into both ears. **LFE is included at gain 0.5** (half the
   surround mix contribution): omitting it makes explosions sound thin and
   generates undiagnosable "sounds empty" reports; full LFE gain buries
   dialogue. The matrix for 5.1 (indices FL FR FC LFE BL BR) is:

   ```
   pan=stereo|c0=0.707*c0+1.0*c2+0.707*c4+0.5*c3|c1=0.707*c1+1.0*c2+0.707*c5+0.5*c3
   ```

   7.1 adds side surrounds at the same surround coefficient. Mono/stereo
   sources skip the filter. Layouts outside these tables fall back to
   `-ac 2` with a logged warning (better than silence).

4. **Multi-track: restart-on-switch via a fresh session.** Switching audio
   POSTs a new session at the current position with a different
   `audioTrackId` and DELETEs the prior session. It does **not** wipe
   segments inside the seek path. Seek retains prior-window segments on
   purpose (Gate 2); an audio switch must not (init and segments carry the
   old audio config). Overloading one path with two retention policies is how
   subtle bugs arrive. More HTTP round trips, less cleverness.

5. **Revisit trigger.** Restart-on-switch stays while warm mid-playback
   switch time stays inside Gate 2's three-second seek budget on reference
   hardware. Measure cold (session start with a non-default track) and warm
   (switch during playback). If warm switches consistently exceed 3s, or the
   cost is source-read dominated rather than encode-startup dominated,
   alternate HLS AUDIO renditions get their own ADR. A preference without
   that threshold decays.

6. **Track identity (Rule 4.9 / 4.11).** Embedded audio uses
   `trackId = e{streamIndex}` (absolute ffprobe index), the same scheme as
   embedded subtitles (ADR-0010). Inventory shape:
   `{ trackId, language?, codec, channels, channelLayout?, label?, default,
   streamIndex }`. Listed in `playbackInfo.audioTracks` the same for
   `directPlay`, `remux`, and `transcode`. The client asks for a track; it
   never reasons about delivery path to find tracks. Direct play: switch is
   client-side and free when the selected track is within the channel
   ceiling. Selecting an over-ceiling secondary on an otherwise DirectPlay
   title (stereo default, 5.1 commentary) starts a hybrid session so the
   downmix still runs. Sessions: `POST .../sessions?startMs=&audioTrackId=`.
   **This is the established pattern for anything with tracks** — identity
   scheme, inventory shape, listed-uniformly — so the next track-bearing
   feature inherits it.

7. **Probe shapes.** First-audio `audio_channels` is stored on `media_items`
   (append-only migration) so `decide_playback` can apply the channel ceiling
   without a live probe. Full per-track inventory is probed on demand at
   playback-info time (same as embedded subtitles). No audio-sidecar table.

8. **API.** OpenAPI adds `AudioTrack` and `PlaybackInfo.audioTracks`; session
   start accepts optional `audioTrackId` (default = flagged default / first).
   Spec and implementation land in the same commit. v0 remains unfrozen.

## Consequences

**7.1 DirectPlay narrows.** A 7.1 AAC MP4 that DirectPlays today becomes a
session tomorrow (hybrid video-copy + stereo encode). It stops being instant
and starts consuming a cap slot. That is correct — browsers cannot render 7.1
usefully — but it is a user-visible downgrade for anyone with 7.1 sources, and
the first time the capability profile has narrowed DirectPlay rather than
widened it. The later widened-`BROWSER_V0` slice (HDR, more codecs, richer
layouts where the client can take them) restores some of that ground.

**Lost.** Instant DirectPlay of multi-channel AAC in MP4/M4V when channels
exceed the profile ceiling. In-place audio switch without a new session.

**Gained.** Dialogue-preserving downmix. Reachable secondary audio tracks.
One switch model for remux and transcode. Hybrid avoids pointless video
re-encodes. Seek retention policy stays single-purpose.

**Corpus.** Existing 7.1 and mono fixtures verify the ceiling and passthrough.
New synthetic multi-language and commentary MKVs cover switch. Dialogue
audibility of a 5.1→stereo downmix is a listen check, not a CI assert: a
centre-only 5.1 tone through the pan matrix lands equal energy in both stereo
channels (~−21 dB RMS), which is the automated proxy; someone still has to
hear a real title.

**Measured switch (corpus multilang MKV, local disk).** Cold session start
0.11 s; warm mid-playback switch to `e2` (fresh POST + first segment) 0.11 s —
inside the 3 s revisit budget on this hardware. NAS multi-track titles should
be re-checked when dogfooding.
