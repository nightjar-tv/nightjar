# Playlist shape probe — results

Static HLS only (no Nightjar server). Three shapes, real fMP4 windows from
Rick and Morty S09E04 (video copy; audio AAC for Chrome MSE). Title duration
for the UI bar is ~1325 s. Land A ≈ 8–48 s media; land B ≈ 598–639 s.

Harness: `scripts/playlist_shape_probe/` (`build.sh`, `serve.py`, `page.html`,
`avplayer_probe.swift`).

## Shapes

| ID | Contract |
|---|---|
| A | One EVENT URI; `POST /mutate` rewrites body from land A → land B |
| B | Fresh EVENT URI per land (`shape_b_land_{a,b}.m3u8`) |
| C | Fresh VOD+ENDLIST URI per land (`shape_c_land_{a,b}.m3u8`) |

## AVPlayer (macOS)

| Trial | Seekable before | Seek result |
|---|---|---|
| B EVENT land A, seek 20 | `[0, ~27]` | lands ~20, plays |
| B EVENT land A, seek 600 | `[0, ~27]` | **fails** (`seek_finished=false`), stays near start |
| B EVENT land B, seek 610 | `[0, ~27]` | **fails**; window is still ~40 s, zero-based |
| C VOD land A, seek 20 | `[0, ~42]`, duration ~42 | lands 20, plays |
| C VOD land A, seek 600 | `[0, ~42]` | **clamps to end** (~42), rate 0 |
| C VOD land B, seek 610 | `[0, ~42]` | **clamps to end** |
| A mutate → new player on same URI, seek 610 | land B window `[0, ~27]` | **fails** (same as B) |

Title-absolute `EXT-X-START` on a short window does **not** make seekable
cover the title. Seekable length ≈ sum of listed `EXTINF`.

## hls.js (Chrome headless)

Same pattern (AAC rebuild):

| Trial | After seek |
|---|---|
| B EVENT land A, seek 20 | ~21, playing |
| B EVENT land A, seek 600 | **clamps** to ~42 |
| B switch URI → land B, seek 20 | ~21 in **new** window (mid-title media) |
| C VOD land A, seek 600 | **clamps** to ~43 |
| C switch URI → land B, seek 20 | 20 in new window |
| A live mutate then seek 600 | duration becomes land B (~45); seek **clamps** to end |
| A live mutate then seek 20 | ~21 in new window |

## Verdict

1. Drawing a scrubber from item duration does **not** make native/`currentTime`
   seeks into unlisted title time work. That is the ADR-0016 failure mode.
2. Far seek requires loading a playlist whose listed media covers the target.
   That means a **new playlist URI (or equivalent reload of a new body at a
   distinct URL)** after `?startMs=` / restart — not mutating one EVENT under
   a live player as the seek mechanism.
3. EVENT vs windowed VOD: both expose a **window-local** timeline. Prefer
   **EVENT while a run is cooking** (append real EXTINF), **ENDLIST when that
   run hits EOF**. Scrub to another region = new run + new playlist URI.
4. Do not put title-absolute offsets in `EXT-X-START` on a short window;
   keep START window-relative. Title land is a session API field (`landedMs`).

Safari native in this harness was not separately automated (AVPlayer is the
AVFoundation path those engines share for HLS). Manual Safari pass on
`page.html` is optional confirmation; AVPlayer + hls.js already agree.
