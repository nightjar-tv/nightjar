# ADR-0010: Text subtitle tracks as WebVTT sidecars

- Status: accepted
- Date: 2026-07-26

## Context

Remux and HLS map only the first video and first audio stream (ADR-0006 /
ADR-0007), so every embedded subtitle track is dropped. Direct play can carry
soft subs in some containers, but the web player has no track list and no
`<track>` elements. Household libraries commonly have SRT (and ASS/PGS) in
MKV; without a sidecar path, remux playback is silent on dialogue.

Burn-in for image and styled ASS tracks is later Phase 2 work. What is
irreversible now is the public track list shape, the VTT URL, and the on-disk
cache key (Rule 6.1 / 4.9).

## Decision

1. **Sidecar WebVTT from the original file.** Extract text subtitle streams
   from `media_items.path` with FFmpeg into WebVTT. The same extraction serves
   direct play and remux. Do not remux subtitle streams into the MP4 cache;
   remux stays video+audio stream-copy.

2. **Text codecs only in this slice.** Extractable: `subrip`, `srt`,
   `webvtt`, `mov_text`, `text`. Not extractable here: `ass`, `ssa`,
   `hdmv_pgs_subtitle`, `dvd_subtitle`, `dvb_subtitle` (burn-in later).
   Unknown subtitle codecs are omitted from the track list with no error.

3. **On-demand inventory, no schema migration.** Playback-info probes subtitle
   streams with ffprobe when the item is direct play or remux. Nothing new is
   stored on `media_items`. Stream identity is the ffprobe stream `index`
   (absolute), stable for a given file version.

4. **Cache.** Extracted VTT files live under
   `{NIGHTJAR_DATA_DIR}/cache/subs/{itemId}-{mtimeMs}-{sizeBytes}-{streamIndex}.vtt`.
   Same identity idea as remux: a changed source misses the cache. No byte cap
   in this slice (VTT is tiny relative to remux); add a cap if abuse appears.
   Extraction runs on first GET of that track, not on every playback-info.

5. **API.** `PlaybackInfo.subtitleTracks` is an array (possibly empty) of
   `{ streamIndex, codec, language?, label?, url }` present for `directPlay`
   and for `remux` (including while remux is preparing: tracks come from the
   source). Omitted or empty for `transcode` until HLS burn-in or sidecar
   work. `GET /api/v0/items/{itemId}/subtitles/{streamIndex}.vtt` returns
   `text/vtt`. A wrong index or non-text codec is 404.

6. **Client.** The item page attaches `<track kind="subtitles">` for each
   listed track when the video element is used for direct play or remux.
   Transcode/HLS players stay without tracks in this slice. The remux
   "subtitles not shown yet" copy goes away when tracks are present.

## Consequences

ASS/PGS dialogue remains invisible on remux until burn-in. Probing subtitle
streams on every playback-info adds a short ffprobe; acceptable for v0. Cache
files are not swept on item delete yet (orphan VTTs age with the data dir).
External `.srt` next to the media file is out of scope here (sidecar-on-disk
discovery is a separate decision).
