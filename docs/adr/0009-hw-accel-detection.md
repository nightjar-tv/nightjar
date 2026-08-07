# ADR-0009: Hardware encode detection by verification

- Status: accepted
- Date: 2026-07-26
- Amended: 2026-08-03 — session-shaped verify; one encode-leg builder; pix_fmt
  ownership; supersede “land backend flags later” (§6). Research pointer:
  `nightjar-meta/notes/hw/jellyfin-hw-encode-map-2026-08.md`.

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

A second force (2026-08): verify and HLS session must not be two FFmpeg graphs
for one encode. AMD VAAPI verified at startup then failed in session (exit 218)
because session applied a global hardware path (`-c:v h264_vaapi` with
software `-pix_fmt yuv420p` and no device/upload) while verify used device +
`hwupload`. That is one concept, two paths (Rule 4.11 / 4.9). Jellyfin’s
multi-vendor helper shows that device bind and surface prep are part of the
encode leg; Nightjar does not adopt their brand dropdown or toggle surface
(Continuity standing review on defaults-before-settings; not a numbered
constitution rule until ENGINEERING_RULES is amended).

## Decision

1. **Verify, do not trust the feature list.** At process startup the transcode
   crate enumerates H.264 encoder candidates relevant to this OS, then for each
   one that FFmpeg advertises runs a **session-shaped** short encode and checks
   that the output demuxes. Advertise-only without a successful encode+demux is
   `failed` with a reason string. Candidates the binary does not list are
   `unavailable`, not failures.

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

   A backend is **preferred only if session-shaped verify succeeds**. It is not
   preferred on a simplified probe that the session cannot reproduce.

4. **`libx264` is always a candidate and always the fallback.** If every
   hardware candidate fails or is unavailable, HLS uses software. No operator
   brand dropdown and no settings UI for acceleration type in this design
   (Rule 4.7; Continuity defaults-before-settings standing review). Escape
   hatches (force software, pin render node), if ever needed, are named in an
   ADR after dogfood proves auto-pick wrong—not invented as speculative knobs.

5. **API readout.** `GET /api/v0/system/transcode` returns FFmpeg version (when
   known), the preferred H.264 encoder name, and per-candidate `name`,
   `backend`, `status` (`verified` | `failed` | `unavailable`), and `reason`
   (required when not verified; null when verified). Additive fields that report
   truth (e.g. which device path verified) may land with the shared-builder
   slice. This is not the liveness `/api/health` route. Phase 1–2 have no auth,
   so the route is public for now. Once Phase 3 auth lands it becomes
   **admin-only**; leaving it public would disclose host encode capability to
   every client. Playback settings UI and `nightjar doctor` consume the same
   detection.

6. **One encode-leg builder for verify and HLS (supersedes prior §6).**  
   **Superseded (2026-07-26 §6):** “HLS uses the preferred encoder for `-c:v`;
   extra per-backend flags land when a backend needs them for real sessions;
   until then a candidate that needs mandatory device args and cannot encode
   with defaults stays failed.” That wording allowed verify and session to
   diverge and treated device/upload as a later bolt-on.

   **Current:**

   - Probe and HLS session call the **same** function to build the **encode leg**
     of the FFmpeg argv (Rule 4.11).
   - **Encode-leg proof criterion:** for the preferred backend, probe and
     session share the same encode-leg argv: device/init args, upload or filter
     suffix, encoder name, encoder extras, and **pix_fmt policy**. Software
     prefilters in the probe may be a fixed stub (e.g. lavfi) only if they do
     not change that encode-leg contract. A prefilter that changes surface
     format before upload is part of the encode leg and must match.
   - Backend-specific args are the definition of “verified,” not a follow-up
     feature. A candidate that cannot pass session-shaped verify is not
     preferred.
   - The builder (or a small backend **data row**, not a second code path) owns
     whether `-pix_fmt yuv420p` (or equivalent) is applied. Session code must
     **not** apply a global `-pix_fmt yuv420p` to every non-`libx264` encoder.
     Implementers delete that global branch; they do not stack `hwupload` next
     to it (Rule 4.8 / 4.5).
   - First implement field budget (Rule 4.7): encoder name; device/init args;
     upload/filter suffix; pix_fmt policy; fixed encoder extras; fail reason.
     Probe tries candidate render nodes (and CUDA indices when relevant) and
     records which path verified; do not hardcode `renderD128` as the only
     bind under the new builder. No surface-domain type system, HW scale
     filters, or zero-copy decode until a second concrete use case forces them.
   - Audio stays AAC software. Incomplete product shape remains: software
     decode; software scale / tonemap / burn as already designed. Mid-stream
     hardware failure falling back mid-session is still a later slice.
   - Packaging: claims about the product Docker image must match the Dockerfile
     FFmpeg and drivers. Session-shaped verify for image-backed claims runs
     against that image’s FFmpeg, not only a host build.

7. **HLS uses the preferred backend’s full encode leg**, not only the encoder
   name string. Preferred means “session builder would succeed.”

8. **HEVC hardware encode is out of this ADR.** v1 browser playback still
   targets H.264 for transcode. HEVC can extend the same verify pattern later.

9. **Support matrix is a published doc, not an API field.** See
   [docs/HW_ACCEL.md](../HW_ACCEL.md). Tiers are operational claims about what
   the team has verified on real hardware. The API reports what *this process*
   verified; the matrix reports what *we* stand behind.

10. **Presence is not capacity.** A device or ICD that enumerates (DRM node,
    Vulkan ICD, encoder name in `ffmpeg -encoders`) must still pass the encode+
    demux verify before it is `verified`. Software paths that present a device
    but cannot sustain realtime (e.g. Mesa lavapipe ~0.30× on the 2026-08-02
    libplacebo spike host) must not be promoted as if they were usable encode
    capacity. Detection answers “can this process encode”; sizing and Gate
    concurrency floors answer “how many streams” — keep those claims separate.
    Spike pointer: `nightjar-meta/notes/hw/libplacebo-dv-spike-2026-08-02.md`.

## Consequences

Startup grows by the cost of a few short encodes once. Session-shaped verify may
be slightly heavier than a minimal lavfi-only graph; that is intentional so
preferred never lies. A hung hardware driver can delay listen until the
per-candidate timeout fires; that timeout is part of the implementation, not a
user setting. Containers without device passthrough correctly report
software-only.

The shared-builder implementation must delete the dual verify/session graphs
and the global non-x264 session `-pix_fmt yuv420p` (Rule 4.5). A provisional
VAAPI-only branch in session spawn is not an acceptable interim product
(Rule 4.8). Measurement binaries used for Gate concurrency are labeled as
evidence, not as main.

Amending the preference policy for VideoToolbox quality is expected once remote
bitrate caps exist; until then LAN throughput wins.

The item-page encoder readout is sourced from the live session, not the startup
preference, so it stays accurate when fallback changes a session's encoder.
Mid-stream fallback does not exist yet; today the field reports the encoder
selected when the session starts, and its meaning becomes "current encoder"
when fallback lands.

Hardware-over-`libx264` is policy, not incidental sort order: hardware is
preferred for throughput and power. VideoToolbox quality at low bitrates is the
known exception that later quality-tuning work may amend.

`GET /api/v0/system/transcode` is public only because Phase 2 has no auth. It
becomes admin-only in Phase 3 because it discloses server capability.

This advances ADR-0007's deferred hardware work for encode selection and encode
leg construction. Decode `-hwaccel` and remux remain unchanged until a second
use case justifies them (Rule 4.7).

Enumerated-but-slow software Vulkan (lavapipe) is a reminder that verify and
realtime capacity are different claims; do not treat ICD presence as a Gate 2
concurrency floor.

Research only (not requirements): `nightjar-meta/notes/hw/jellyfin-hw-encode-map-2026-08.md`.
