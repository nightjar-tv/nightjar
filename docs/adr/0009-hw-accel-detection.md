# ADR-0009: Hardware encode detection by verification

- Status: accepted
- Date: 2026-07-26

## Context

Phase 2 needs hardware H.264 encode when the machine can actually do it.
FFmpeg's encoder list lies routinely: the binary advertises VAAPI or NVENC
while `/dev/dri` is missing, the Nvidia runtime is absent, or VideoToolbox is
present but unusable in this process. A feature-list probe would report
capabilities the session path cannot use.

ADR-0007 ships software `libx264` only. The plan requires detection-by-
verification, a tiered support matrix, and a readout users (and later
`nightjar doctor`) can trust. The irreversible parts are the public capability
shape and the verify contract, so they are decided here before code
(Rule 6.1 / 4.9).

## Decision

1. **Verify, do not trust the feature list.** At process startup the transcode
   crate enumerates H.264 encoder candidates relevant to this OS, then for each
   one that FFmpeg advertises runs a short lavfi encode (about two seconds of
   320×240 testsrc) and checks that the output demuxes. Advertise-only without
   a successful encode+demux is `failed` with a reason string. Candidates the
   binary does not list are `unavailable`, not failures.

2. **Probe once per process, never per session.** Results are held in memory on
   `AppState` for the process lifetime and reused by every HLS session start.
   Re-running the verify encodes on playback would add seconds before first
   frame for no benefit. The same `probe_h264_encoders` entry point is what
   `nightjar doctor` will call later to refresh or report; sessions never
   trigger a probe.

3. **Preference policy (amendable).** Among verified candidates, pick the first
   in this order for the host OS:
   - macOS: `h264_videotoolbox`, then `libx264`
   - Linux: `h264_nvenc`, `h264_qsv`, `h264_vaapi`, `h264_v4l2m2m`, then
     `libx264`
   - Windows: `h264_nvenc`, `h264_qsv`, `h264_mf`, then `libx264`

   Throughput preference is intentional for this slice: a verified hardware
   encoder always beats software when both work. That is wrong for quality in
   at least one known case (VideoToolbox at low bitrates trails x264). Do not
   special-case it here. Later quality-tuning amends this policy (per-backend
   bitrate/CRF defaults, or demoting a backend below `libx264` under a bitrate
   ceiling) rather than inventing an implicit sort in code.

4. **`libx264` is always a candidate and always the fallback.** If every
   hardware candidate fails or is unavailable, HLS uses software. No config
   knob in this slice (Rule 4.7).

5. **API readout.** `GET /api/v0/system/transcode` returns FFmpeg version (when
   known), the preferred H.264 encoder name, and per-candidate `name`,
   `backend`, `status` (`verified` | `failed` | `unavailable`), and `reason`
   (required when not verified; null when verified). This is not the liveness
   `/api/health` route. Phase 1–2 have no auth, so the route is public for now.
   Once Phase 3 auth lands it becomes **admin-only**; leaving it public would
   disclose host encode capability to every client. Playback settings UI and
   `nightjar doctor` consume the same detection.

6. **HLS uses the preferred verified encoder for `-c:v`.** Audio stays AAC
   software. Extra per-backend flags (VAAPI device paths, NVENC presets,
   VideoToolbox bitrate quality) land when a backend needs them for real
   sessions; until then a candidate that needs mandatory device args and cannot
   encode with defaults stays `failed` with that reason. Mid-stream hardware
   failure falling back mid-session is a later slice.

7. **HEVC hardware encode is out of this ADR.** v1 browser playback still
   targets H.264 for transcode. HEVC can extend the same verify pattern later.

8. **Support matrix is a published doc, not an API field.** See
   [docs/HW_ACCEL.md](../HW_ACCEL.md). Tiers are operational claims about what
   the team has verified on real hardware. The API reports what *this process*
   verified; the matrix reports what *we* stand behind.

## Consequences

Startup grows by the cost of a few short encodes once. A hung hardware driver
can delay listen until the per-candidate timeout fires; that timeout is part of
the implementation, not a user setting. Containers without device passthrough
correctly report software-only. Amending the preference policy for VideoToolbox
quality is expected once remote bitrate caps exist; until then LAN throughput
wins.

This advances ADR-0007's deferred hardware work for encode selection only.
Decode `-hwaccel` and remux remain unchanged.
