# AMD dogfood box — Docker / packaging requirements

**Date:** 2026-08-03  
**Host:** `nightjar-dogfood` / `192.168.1.184`  
**CPU/GPU:** AMD Ryzen 5 5500U, Radeon Graphics (Lucienne / Renoir, `radeonsi`)  
**Purpose:** what to install and pass into a Nightjar container so VAAPI encode
works on this class of box. Evidence from encode-leg and concurrency spikes
same day; not a product Dockerfile change by itself.

Related: `notes/hw/encode-leg-spike-2026-08-03.md`,
`notes/hw/concurrency-ceiling-amd-2026-08-03.md`, ADR-0009 (session-shaped
encode leg).

## What worked on the host (bare binary)

| Item | Value |
|---|---|
| OS | Ubuntu 26.04 LTS |
| FFmpeg | `8.0.1-3ubuntu2` (distro package) |
| User groups | `video`, `render` |
| DRM | `/dev/dri/card1`, `/dev/dri/renderD128` (only render node) |
| VA driver | Mesa Gallium **radeonsi** (`radeonsi_drv_video.so`) |
| vainfo | VA-API 1.23; H.264 EncSlice present |
| Encode leg | `-vaapi_device /dev/dri/renderD128` **or** `-init_hw_device vaapi=va:/dev/dri/renderD128` (+ optional `-filter_hw_device va`); always `format=nv12,hwupload` before `h264_vaapi` |
| Must not | global session `-pix_fmt yuv420p` into `h264_vaapi` without upload (exit 218) |

Host packages that matter for this path:

```text
ffmpeg
vainfo                    # diagnosis only
mesa-va-drivers           # radeonsi VA
```

Not required for AMD VAAPI:

```text
intel-media-va-driver
i965-va-driver
libmfx-gen1.2 / oneVPL    # Intel QSV only
```

## Docker status on this box (2026-08-03)

- **Docker was not installed** when spikes ran. Product-image container tests
  for AMD Mesa were **not** run on this host.
- Root filesystem was briefly `emergency_ro` during one spike; use a healthy
  rw root before installing Docker or writing under `/home`.

## What a container must include (AMD VAAPI)

When packaging or documenting `docker run` for this box (and similar Renoir /
Rembrandt / Phoenix iGPUs):

### Image contents

1. **FFmpeg** with `h264_vaapi` and `hwupload` (Debian bookworm package is the
   product Dockerfile baseline; version must actually talk to the host kernel’s
   DRM — see packaging caveat below).
2. **`mesa-va-drivers`** (provides `radeonsi_drv_video.so`). Required for AMD.
3. Optional: `vainfo` only if the image is used for operator diagnosis.
4. Do **not** rely on Intel-only packages for AMD encode. Shipping
   `intel-media-va-driver` / `i965-va-driver` in the same image is fine for
   multi-host images (current product Dockerfile already does) but they do
   not replace Mesa on AMD.

### Runtime

```bash
docker run --rm \
  --device=/dev/dri \
  -v /path/to/media:/media:ro \
  -v /path/to/config:/config \
  -e NIGHTJAR_DATA_DIR=/config \
  nightjar/nightjar
```

Requirements:

| Runtime | Why |
|---|---|
| `--device=/dev/dri` (or at least `renderD128` + needed card node) | VAAPI open |
| Container process can open render node | Usually root in image, or group matching host `render`/`video` |
| No need for NVIDIA toolkit / CUDA | AMD path is VAAPI/Mesa |
| No need for `/dev/dri` omit “software only” expectation | Without DRI, preferred must fall back to libx264 |

Encode-leg after shared builder (ADR-0009): probe records which
`/dev/dri/renderD*` verified; do not hardcode D128 as the only bind if more
nodes appear.

### Host prep checklist (before first container)

```bash
# packages
sudo apt-get install -y docker.io   # or Docker CE; not present 2026-08-03
sudo apt-get install -y mesa-va-drivers ffmpeg vainfo   # if bare-metal too

# groups (for non-root container user later)
sudo usermod -aG docker,render,video "$USER"   # re-login

# sanity
ls -l /dev/dri
vainfo --display drm --device /dev/dri/renderD128 | head -30
# expect radeonsi + EncSlice for H.264
```

## Product Dockerfile today vs this box

Product `Dockerfile` (bookworm) installs roughly:

```text
ffmpeg
intel-media-va-driver
mesa-va-drivers
i965-va-driver
```

For **this AMD host**, the critical package is **`mesa-va-drivers`**. Intel
packages are unused here.

### Packaging caveat (N150 image spike, same day)

On N150, a bookworm container with those packages + `libmfx-gen1.2` saw
**iHD/i965 init failure** against the host kernel while host Ubuntu FFmpeg 8
worked. That is driver/FFmpeg **version skew**, not an AMD-specific issue.
When building/testing the product image for AMD:

1. Run session-shaped VAAPI verify **inside** the image with `--device=/dev/dri`
   on this box (Docker must be installed first).
2. Confirm `vainfo` inside the container reports **radeonsi**, not a failed
   Intel driver pick.
3. If Mesa in bookworm is too old for the host GPU/kernel, document that the
   bare binary + host FFmpeg path is the supported escape on that OS, or bump
   image base/drivers — do not claim image HW without that measure.

## Encode-leg row to bake into the builder (AMD VAAPI)

Same as Intel VAAPI row from `encode-leg-spike-2026-08-03.md`:

```text
encoder:     h264_vaapi
device:      -vaapi_device {verified_render_node}
             # alternate measured: -init_hw_device vaapi=va:{path}
upload:      format=nv12,hwupload
pix_fmt:     do not set software yuv420p for this encoder
```

## Not in scope for this note

- AMF (Windows-centric; not this Linux iGPU path)
- ROCm / HIP encode
- NVIDIA on this box (no discrete GPU in the spike inventory)
- Shared-builder implementation (separate slice after ADR-0009)

## When revisiting

Install Docker on `192.168.1.184`, re-run product-image VAAPI encode-leg + one
HLS session, and append results here (date, image digest, FFmpeg version inside
container, pass/fail). Until then, host bare-metal VAAPI is proven; image-on-AMD
is **unmeasured**.
