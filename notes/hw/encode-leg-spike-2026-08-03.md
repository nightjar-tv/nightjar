# Encode-leg argv spike (session-shaped rows for ADR-0009)

**Date:** 2026-08-03  
**Purpose:** fill backend **data rows** for the shared encode-leg builder. Not
product code. JF note is refuse-list only; rows come from these measures.

Probe input: short synthetic video (HEVC-in-MP4 or H.264 lavfi). Audio omitted
(`-an`). Encode leg only.

## Proposed backend rows (from measure)

| Backend | encoder | device / init | upload / filter suffix | pix_fmt policy | extras | Notes |
|---|---|---|---|---|---|---|
| software | `libx264` | none | none (SW scale/tonemap/burn only) | `yuv420p` | `-preset veryfast` | Pass all hosts |
| videotoolbox | `h264_videotoolbox` | none | none | `yuv420p` **or omit** (both pass) | none required | Mac 8.1.2 |
| qsv | `h264_qsv` | none for sysmem path | none | `nv12` or `yuv420p` or omit (all pass on N150 host) | none | N150 + oneVPL; no upload |
| vaapi | `h264_vaapi` | `-vaapi_device PATH` **or** `-init_hw_device vaapi=va:PATH` (+ optional `-filter_hw_device va`) | `format=nv12,hwupload` | **must not** force software `yuv420p` | none | Intel host + AMD Mesa |
| nvenc | `h264_nvenc` | none for sysmem path (generic leg) | none for sysmem | `yuv420p` (nv12 / omit also OK raw) | none | 2080 SUPER host 2026-08-03 |

**Render node:** probe tries `/dev/dri/renderD*` and records the path that
verified (ADR-0009). On both measured hosts only `renderD128` was present and
worked.

**Device form:** on AMD host FFmpeg 8.0.1 and N150 host FFmpeg 8.0.1, both
`-vaapi_device` and `-init_hw_device vaapi=va:…` (+ hwupload) pass. Prefer one
form in the builder; either is valid encode-leg for these stacks. Simplest
ship form: `-vaapi_device {path}` (matches existing `verify_vaapi`).

**Broken session shape (current product dual path):**  
`-c:v h264_vaapi -pix_fmt yuv420p` without device/upload → **rc 218**, 0 bytes
(N150 and AMD). Builder must own pix_fmt and delete the global non-x264 branch.

---

## Host: N150 (`nightjar-dev`, Intel N150)

- FFmpeg: `8.0.1-3ubuntu2`  
- `libmfx-gen1.2` installed  
- DRI: `renderD128`  
- Input: `/tmp/spike_hevc.mp4` (short HEVC)

| label | rc | bytes | verdict |
|---|---:|---:|---|
| n150_qsv_sysmem_nv12 | 0 | >0 | pass, no upload |
| n150_qsv_sysmem_yuv420p | 0 | >0 | pass |
| n150_qsv_sysmem_nopix | 0 | >0 | pass |
| n150_qsv_from_vaapi (init vaapi+qsv@va + hwupload qsv) | 0 | >0 | pass (optional richer path; not required for first row) |
| n150_vaapi_device_hwupload | 0 | >0 | pass |
| n150_vaapi_init_hw | 0 | >0 | pass |
| n150_vaapi_broken_session | **218** | 0 | fail (current dual path) |
| n150_libx264 | 0 | >0 | pass |

**QSV Linux:** system-memory encode is enough on this host. Derived QSV-from-VAAPI
also works; do not require it for the first implement unless product-image
probe forces it.

---

## Host: AMD (`nightjar-dogfood`, Ryzen 5 5500U / Renoir)

- FFmpeg: `8.0.1-3ubuntu2`  
- Driver: Mesa Gallium radeonsi (vainfo OK)  
- DRI: `renderD128` only  
- Root was `emergency_ro` during spike; artifacts under `/tmp`  
- Docker **not** installed (no product-image container on this host)

| label | rc | bytes | verdict |
|---|---:|---:|---|
| amd_vaapi_device_hwupload | 0 | >0 | pass |
| amd_vaapi_init_hw | 0 | >0 | pass |
| amd_vaapi_init_hw_nofilter | 0 | >0 | pass |
| amd_vaapi_renderD128 | 0 | >0 | pass (node discovery) |
| amd_vaapi_broken_session | **218** | 0 | fail |
| amd_qsv_sysmem | 171 | 0 | fail (no Intel) — expected |
| amd_libx264 | 0 | >0 | pass |

Earlier same day (before RO root): product session with encode-leg
device+hwupload produced HLS segments; concurrency ceiling **lastOk 5**
(`concurrency-ceiling-amd-2026-08-03.md`).

---

## Mac (Apple Silicon host FFmpeg)

- FFmpeg: `8.1.2` (Homebrew)  
- Encoder: `h264_videotoolbox`

| label | rc | verdict |
|---|---:|---|
| mac_vt_yuv420p | 0 | pass |
| mac_vt_nopix | 0 | pass |
| mac_vt_nv12 | 0 | pass |
| mac_libx264 | 0 | pass |

Pix_fmt policy for VT: either `yuv420p` or omit. Prefer `yuv420p` to match
software SDR output tags, or omit if builder wants minimal flags—both measured OK.

---

## Product-image-like FFmpeg (Debian bookworm stack on N150)

Container: `debian:bookworm-slim` + Dockerfile packages (`ffmpeg`,
`intel-media-va-driver`, `mesa-va-drivers`, `i965-va-driver`) and
`libmfx-gen1.2`. FFmpeg **5.1.9-0+deb12u1**. `--device=/dev/dri`.

| label | rc | note |
|---|---:|---|
| all QSV / VAAPI encode attempts | **1** | `iHD_drv_video.so init failed`; device create -5 |
| LIBVA_DRIVER_NAME=i965 | **1** | i965 init failed too |
| privileged retest | **1** | same iHD init fail |

**Honest packaging:** on this N150 + host kernel, **bookworm VA drivers in the
image do not initialize** while host Ubuntu 26.04 FFmpeg 8 + media driver 26.x
do. That is separate from encode-leg argv shape. Gate/packaging re-verify must
use a host where the **image’s** FFmpeg+drivers work (e.g. Unraid dogfood), or
revisit image driver package versions. Do not claim product-image HW encode on
N150 from this spike.

AMD had no Docker; product-image-on-Mesa not run.

---

## Open items closed by this spike

| Question | Answer |
|---|---|
| QSV Linux sysmem vs derived? | Sysmem sufficient on N150 host; no upload |
| lavfi vs fixture? | Short file or lavfi both fine for encode-leg; used short HEVC file |
| `-vaapi_device` vs `-init_hw_device`? | Both pass on host FFmpeg 8 (Intel + AMD Mesa) |
| AMD VAAPI row? | Same as Intel VAAPI: device + `format=nv12,hwupload`, no SW yuv420p |
| NVENC? | **Done** — see host 192.168.1.33 below |

---

## Host: i3-12100 + RTX 2080 SUPER (`192.168.1.33`, same Ubuntu dogfood image)

- FFmpeg: `8.0.1-3ubuntu2`
- GPU: GeForce RTX 2080 SUPER (TU104)
- Driver: **nvidia-driver-595-open** 595.84 (after install + reboot). First boot
  of this drive on this machine was **nouveau** only — `nvidia-smi` failed and
  `h264_nvenc` could not load `libcuda.so.1` until proprietary modules loaded.
- Nightjar binary: encode-leg build (same as N150/AMD re-verify)

### Raw encode-leg (host FFmpeg)

Sysmem `-c:v h264_nvenc` with `-pix_fmt yuv420p` / `nv12` / omit: **pass**
(session-shaped probe uses the generic HW row: yuv420p, no CUDA init).

Optional CUDA upload path (`-init_hw_device cuda=cu:0` + `hwupload_cuda`)
was not required for first product row; sysmem matches the builder’s
`EncodeLeg::generic_hw` for NVENC.

### Nightjar session (2026-08-03)

| Check | Result |
|---|---|
| preferred | **`h264_nvenc`** (first in Linux preference order) |
| preferredDevice | null (sysmem leg) |
| session videoEncoder | `h264_nvenc` |
| HLS | master + media playlist; segments present (short clip → one long EXTINF OK) |

QSV/VAAPI failed probe on this host as expected (Intel UHD present but VA
drivers not initialized the same way; NVIDIA is preferred and verified).

## Builder status (2026-08-03)

**Done in tree** (`server/crates/transcode/src/hwaccel.rs` + HLS spawn):

- Shared `EncodeLeg` rows for software / VT / QSV sysmem / VAAPI / generic HW
  (NVENC) matching the table above.
- Probe and session share `push_pre_input` / `compose_video_filter` /
  `push_encoder_args`. Dual `verify_vaapi` and global non-x264 session
  `-pix_fmt yuv420p` removed.
- Probe tries `list_render_nodes()` and records the winning device on the
  preferred leg; API exposes optional `preferredDevice`.
- Probe software stub is `format=yuv420p` composed with `upload_vf` (same
  surface handoff sessions may apply before VAAPI upload). See
  `notes/hw/encode-leg-argv-walk-2026-08-03.md`.

**Still open (packaging honesty, not builder shape):**

- Product bookworm image HW encode on N150 failed drivers-init in this spike.
- Product-image-on-Mesa / Unraid image re-verify still required before any
  “image HW encode” claim. Host-binary claims only until then.
- Docker QSV (oneVPL) is not a measured image claim.

