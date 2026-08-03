# Jellyfin hardware encode map (research, 2026-08)

**Purpose:** record how Jellyfin builds multi-vendor FFmpeg graphs so Nightjar
can close the verify-vs-session gap without copying their toggle surface.
**Not product docs.** Local tree: `Documents/GitHub/jellyfin` (same machine).

Related plan decision: amend ADR-0009 in place (session-shaped verify; one
builder). Nightjar product stance: no brand dropdown; Continuity standing
review on defaults-before-settings (cited there as “4.12”; not numbered in
`ENGINEERING_RULES.md` as of this note).

## Where the logic lives

| Area | Path (local jellyfin tree) |
|---|---|
| Brand enum | `MediaBrowser.Model/Entities/HardwareAccelerationType.cs` |
| Operator options | `MediaBrowser.Model/Configuration/EncodingOptions.cs` |
| Argv / filter graphs | `MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs` (~8k LOC) |
| Encoder/filter catalog | `MediaBrowser.MediaEncoding/Encoder/EncoderValidator.cs` |
| Driver / device flags | `MediaBrowser.MediaEncoding/Encoder/MediaEncoder.cs` |

## Operator model (refuse for Nightjar product)

`HardwareAccelerationType`: `none`, `amf`, `qsv`, `nvenc`, `v4l2m2m`, `vaapi`,
`videotoolbox`, `rkmpp`. The operator picks one brand. `EnableHardwareEncoding`
gates use. Device strings default to `VaapiDevice = /dev/dri/renderD128` and
optional `QsvDevice`. Many other knobs (tonemap modes, low-power Intel
encoders, decoding codec lists, enhanced NVDEC, etc.) live on
`EncodingOptions`.

Encoder name is a map from that enum (`h264` + `_nvenc` / `_qsv` / …) in
`GetH26xOrAv1Encoder` (`EncodingHelper.cs` ~212–241). There is no automatic
cross-brand “prefer NVENC over QSV” at runtime; the brand was chosen upstream.

Nightjar refuses this as the primary product surface. Detection should pick a
working backend; settings only as escape hatches if dogfood proves need.

## Device init (FFmpeg reality worth keeping)

Helpers build `-init_hw_device` fragments, not only `-c:v`:

| Backend | Pattern (EncodingHelper) | Approx lines |
|---|---|---|
| CUDA / NVENC | `cuda={alias}:{index}` | `GetCudaDeviceArgs` ~841 |
| VAAPI | `vaapi={alias}:{renderNode\|vendor\|driver opts}` | `GetVaapiDeviceArgs` ~910 |
| QSV Linux | VAAPI with iHD then `qsv={alias}@{va}` | `GetQsvDeviceArgs` ~947–954 |
| QSV Windows | d3d11va then qsv derived | ~956–966 |
| VideoToolbox | `videotoolbox={alias}` | ~833 |
| RKMPP | `rkmpp={alias}` | ~825 |
| Filter device | `-filter_hw_device {alias}` | `GetFilterHwDeviceArgs` ~971 |

Input-side assembly for the selected brand is in `GetInputVideoHwaccelArgs`
(~1011+): VAAPI branches on `IsVaapiDeviceInteliHD` / `i965` / `Amd` and may
insert DRM→VAAPI derive for AMD Vulkan interop.

Nightjar takeaway: device bind is part of the encode leg. A verify that uses
device args while the session does not is two paths for one concept.

## Capability beyond encoder name

`EncoderValidator` lists encoders, decoders, hwaccels, filters, and checks
filter options. It probes VAAPI render nodes with verbose
`init_hw_device vaapi=va:{path}` and classifies driver strings (Mesa Gallium
vs Intel iHD vs i965). `MediaEncoder` stores `IsVaapiDeviceAmd`,
`IsVaapiDeviceInteliHD`, Vulkan DRM interop flags, etc. (~83–87, 155–167,
246–272).

Nightjar’s lavfi encode+demux is stricter for “can produce a file” and weaker
for “which graph is legal.” Closing the gap is session-shaped verify of the
**encode leg**, not importing JF’s full catalog UI.

## Filter graphs (why JF is large)

Separate chains (not one pipeline with a field):

- NVIDIA: `GetNvidiaVidFilterChain` / `GetNvidiaVidFiltersPrefered` (~3975+)
- Intel VAAPI full: `GetIntelVaapiFullVidFiltersPrefered` (~5078+)
- AMD VAAPI full: `GetAmdVaapiFullVidFiltersPrefered` (~5314+)
- VAAPI limited, QSV DX11 / VAAPI-derived, Apple VT, RKMPP: similar blocks

Each returns main / sub / overlay filter lists and accounts for deinterlace,
scale, tonemap, burn/overlay, rotation, and whether frames stay on GPU
(`isCuInCuOut`, `isVaInVaOut`). SW-decoder paths often scale in memory then
`format=nv12` and only upload when needed; full paths keep VAAPI/CUDA surfaces.

Nightjar v1 product incomplete shape: software decode, software scale /
tonemap / burn (already owned), then backend-owned upload + encoder. Zero-copy
decode is a second use case (Rule 4.7), not the first builder.

## What Nightjar was missing (mapped to plan)

| Gap | JF | Nightjar before ADR amend |
|---|---|---|
| Device on session encode | Always when brand needs it | Verify-only for VAAPI; session global HW path |
| Pix fmt policy | Per graph | Global `-pix_fmt yuv420p` for all non-x264 sessions |
| Brand selection | User enum | Prefer order (good) |
| NVENC session proof | Full CUDA path | Name verify only; no team NVENC host measure |
| Multi-render / multi-GPU | User device strings | Hardcoded D128 in verify |
| Driver family | Internal flags | Unused |

2026-08-03 dogfood: AMD VAAPI preferred after verify; session exit 218 with
software yuv420p into `h264_vaapi`. Capacity when graph is correct: see
`notes/hw/concurrency-ceiling-amd-2026-08-03.md` and N150 QSV notes (patched
or manual argv for measure only).

## Refuse list (do not import)

- Operator-facing acceleration brand as primary control.
- Tonemap algorithm / mode / peak settings UI for v1.
- Dual “preferred vs legacy” permanent pipelines.
- AMF/RKMPP product path without hardware and scope.
- Cloning EncodingHelper size as architecture.

## Pointers only (no vendored JF code)

All line numbers are from the local jellyfin tree as of this research date and
will drift. Re-open the named symbols if lines move.
