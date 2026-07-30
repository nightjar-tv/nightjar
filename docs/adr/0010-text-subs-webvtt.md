# ADR-0010: Text subtitle tracks as WebVTT sidecars

- Status: accepted
- Date: 2026-07-26

## Context

Remux and HLS map only the first video and first audio stream (ADR-0006 /
ADR-0007), so every embedded subtitle track is dropped. Direct play can carry
soft subs in some containers, but the web player has no track list and no
`<track>` elements. Household libraries commonly have SRT (and ASS/PGS) in
MKV, and keep external `.srt` / `.vtt` next to the video. Without a sidecar
path, remux playback is silent on dialogue.

Burn-in for image and styled ASS tracks is later Phase 2 work. What is
irreversible now is the public track list shape, the VTT URL identity, the
on-disk cache key, and how filesystem sidecars are stored after the index
pass (Rule 6.1 / 4.9).

## Decision

1. **One track type.** Embedded and filesystem sidecar tracks share one
   `SubtitleTrack` shape. They differ only by `source` (`embedded` |
   `sidecar`). No parallel inventory or serve paths that both mean "a
   subtitle."

2. **Stable track identity (Rule 4.9).** The URL is
   `GET /api/v0/items/{itemId}/subtitles/{trackId}.vtt`. `trackId` is not a
   list ordinal:
   - Embedded: `e{streamIndex}` where `streamIndex` is the absolute ffprobe
     stream index (e.g. `e2`).
   - Sidecar: `s` plus an optional `-{suffix}` derived from the filename after
     the video basename (and a `Subs.` / `Subtitles.` token when the file
     lives in those sibling directories). Examples for `Movie.mkv`:
     `Movie.srt` → `s`; `Movie.en.srt` → `s-en`; `Movie.en.forced.srt` →
     `s-en.forced`; `Subs/Movie.en.srt` → `s-Subs.en`.
   Adding `Movie.fr.srt` must not renumber an existing selection a client
   remembered. The id carries no extension, so `Movie.en.srt` and
   `Movie.en.vtt` collide on `s-en`; discovery keeps one deterministically
   (vtt over srt over ass over ssa, the servable format wins) and logs the
   skipped file.

3. **WebVTT delivery.** Text streams become WebVTT for direct play and
   remux. Do not remux subtitle streams into the MP4 cache; remux stays
   video+audio stream-copy. Embedded text codecs extractable here:
   `subrip`, `srt`, `webvtt`, `mov_text`, `text`. Sidecar `.vtt` is served
   as-is; sidecar `.srt` converts on read. ASS/SSA (embedded or sidecar) and
   image subs (PGS, etc.) are listed when discovered but not served; burn-in
   is later. Unknown embedded subtitle codecs are omitted with no error.

4. **Filesystem sidecar discovery at index time.** The scanner associates
   sidecars during the index pass and stores them on
   `media_item_sidecars`. Playback reads that table; it does not stat the
   media directory. The library watcher already triggers a rescan, so a
   subtitle added beside a video is picked up without a manual rescan.
   Sidecar extensions are never media items.

5. **Matching convention.** Same basename as the video, with optional
   language and flag suffixes before the extension; also the same names
   inside a `Subs/` or `Subtitles/` sibling directory. Extensions in scope
   for discovery: `.srt`, `.vtt`, `.ass`, `.ssa`. Language tokens are two-
   or three-letter codes, normalised to ISO 639-1 when known (small static
   table, no ISO crate). An unrecognised non-flag suffix yields an
   unlabelled track (kept, `language` null). `forced` and `sdh` are boolean
   flags on the track because forced selection differs from full dialogue.

6. **Embedded inventory stays on-demand.** Playback-info still probes
   embedded text streams with ffprobe for direct play and remux. Nothing
   about embedded streams is written to `media_items`. Sidecars are the
   rows that must survive without a directory walk at play time.

7. **Cache.** Extracted and converted VTT files live under
   `{NIGHTJAR_DATA_DIR}/cache/subs/` with identity including item id, source
   mtime/size, and `trackId`. Byte-capped LRU from day one
   (`NIGHTJAR_SUBS_CACHE_BYTES`, default 512 MiB), same idea as remux.
   Embedded text tracks are stream-copied to SRT in one FFmpeg pass (all
   missing tracks together), then converted in-process with the same
   `srt_to_webvtt` path sidecars use. Conversion and extraction run on first
   GET via `spawn_blocking`, with a kill timeout on the FFmpeg child. When remux
   or an HLS session starts, embedded tracks are warmed in the background so
   the first caption request does not race a cold NAS demux alone.

8. **API.** `PlaybackInfo.subtitleTracks` is an array of
   `{ trackId, source, codec, language?, label?, forced, sdh, url?, streamIndex? }`.
   `url` is present only when the track is served as WebVTT. Listed the same
   way for `directPlay`, `remux`, and `transcode`. NFO and other non-subtitle
   sidecars are out of scope (Phase 3 metadata import).

9. **Delivery skins.** Progressive `<video>` takes subtitles via `<track>`;
   HLS takes them via master-playlist `EXT-X-MEDIA` (TYPE=SUBTITLES) pointing at
   a one-segment subtitle media playlist that references the same item VTT URL.
   Two skins, one inventory (Rule 4.11). The media playlist stays at
   `index.m3u8`; the master is `master.m3u8` (ADR-0008 additive test held).

10. **Client contract.** `playbackInfo` always lists tracks the same way
    regardless of playback method. Clients use whatever their player needs
    (`<track>` vs HLS MEDIA). The API never asks a client to reason about
    which delivery world it is in (Rule 2.1). Phase 4 Flutter work settles on
    this before writing client subtitle UI.

11. **Client (web).** Direct play attaches `<track kind="subtitles">` for each
    listed track with a `url`. Remux and transcode both play as HLS sessions
    and take subtitles from the master playlist (ADR-0011); native / hls.js
    caption menus are the control surface (custom picker later).

## Consequences

ASS/PGS burn-in is decided in [ADR-0018](0018-subtitle-burn-in.md): listed
with `render: burnIn`, selected via `subtitleTrackId` on session start.
Probing embedded subtitle streams on every playback-info adds a short
ffprobe; acceptable for v0. Cache files are not swept on item delete yet
(orphan VTTs age under the cap).
External NFO / artwork sidecars are Phase 3 metadata-import work, not this
slice. SRT files that are not UTF-8 are decoded with a Windows-1252 fallback
after a strict UTF-8 attempt so a bad encoding fails closed to legible text
rather than mojibake-as-UTF-8.

Subtitle extraction is stream-copy (or subtitle-to-SRT remux for mov_text /
embedded WebVTT) plus in-process conversion. FFmpeg's WebVTT muxer was the
measured bottleneck: on a NAS-hosted remux title, `-c:s webvtt` lagged far
behind `-c:s copy -f srt` for the same demux. Embedded and sidecar tracks
share one converter (Rule 4.11).

Extraction still shares remux's whole-file-artifact shape: the first request
pays for a demux of the source, then hits a named cache object. Cache warming
on remux start races the demux against the stream-copy so captions are ready
when the MP4 is. Measured dogfooding showed that race loses at real NAS file
sizes: warm did not finish inside the remux window on completed large titles.
The remux warm path stays functional and is not invested in further; warming
moves to session start when remux converges onto HLS (ADR-0011). On transcode
there is no remux-completion moment; extraction competes with the encode for
source reads on cold titles. Measured on a NAS-hosted DTS MKV (item 1, 500
Days of Summer) with a live HLS encode of the same file: cold extract ~255s
to first WebVTT byte; cache hit ~0.08s. Copy-deck honesty ("captions may take
a moment") is grounded in that cold figure, not the remux Birder ~90s number.

HLS sessions snapshot serveable tracks at start. A sidecar added while a
session is live does not appear in that session’s master; it appears on the
next session after rescan. That is a consequence of the snapshot, not a bug.

Gate 3: Tizen and webOS ship their own HLS implementations. This slice verifies
Chrome (hls.js) and Safari (native). TV-browser parity is untested here and is
a Gate 3 risk — the master playlist increases what those stacks can get wrong.

Convergence onto range-addressable operations is a candidate, not a plan, and
needs measured dogfooding evidence to justify. Moving remux onto HLS would not
reduce extraction cost — the cost is the source demux, not the delivery
model.
