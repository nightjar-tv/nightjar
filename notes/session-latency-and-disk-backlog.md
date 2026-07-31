# Session latency and disk backlog (refile)

Status: still open; priority refiled 2026-07-31 after engine bake-off Step 1/1b.

## Priority

**Matters for remote (and any bandwidth-forced session), not for ~80% of LAN
playback under an engine client.**

Under `MPV_V0` / `VLC_V0`, compatibility-transcode is ~0%. Bandwidth-transcode
is the remaining session load: ~21% of this library exceeds an 8 Mbps ceiling,
~2% exceeds 15 Mbps (see `notes/client-arch/engine-bakeoff.md` Step 1b). Far-
seek cook latency (p50 roughly 3–5 s on the ADR-0020 build) is the scrub
experience those viewers get. Do not demote the work to "nice to have"; do not
keep framing it as the cost of 84.5% of all playback.

LAN engine direct play does not hit this path. Web and bitrate-capped remote
do.

## Measurements

Dogfood numbers live in `nightjar-meta/docs/RESTART_LATENCY_NOTES.md` and
`nightjar-meta/notes/far-seek-baseline-2026-07-31.*` until folded here.

## Still behind keyframe map

Land selection, damage detection at scan, byte offsets, and trickplay stay
ahead of restart-latency tuning. ADR-0020 stands.
