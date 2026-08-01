# ADR-0023: Keyframe map and byte-offset session start

- Status: accepted
- Date: 2026-08-01
- Gate: Phase 2 / Gate 2 (priority over remaining ADR-0022 profile work;
  schemas stay separate)
- Related: [ADR-0020](0020-copy-mode-segment-boundaries.md) (`landedMs`),
  [ADR-0022](0022-capability-profiles.md) (client reports; this is how the
  server reads), [ADR-0013](0013-subtitle-extraction-at-scan.md) /
  [ADR-0019](0019-ass-burn-extract-at-scan.md) (derived artifacts)

## Context

Gate 2 requires seek into an untranscoded region to start playback in under
3 seconds. On the household NAS, cold far seek on a healthy Matroska title
measured about **7.1 s** wall (transcode, first progress) — not an ADR-0020
regression and not cured by playlist swap (~60 ms POST + ~15 ms GET once
mapped). Decomposition showed the cost is FFmpeg rediscovering container
structure on open (`-ss` over SMB), not the encode and not the session swap.

Opening at a known **Cluster byte offset** (header plus body from that
Cluster, no `-ss`) lands cold under 3 s on the purge matrix, with exact
`sidx` on transcode. Copy needs the map’s Cluster PTS as
`-output_ts_offset` rather than the scrub request; that closed far copy from
**+1.42 s** to **+0.083 s**. The residual matches the ~83 ms edit-list /
priming offset already characterised in the cut-rule gating work
(`nightjar-meta/notes/cut-rule-gating-2026-07-31.md`); cite it, do not leave
it unexplained.

That elevates the keyframe map from a Phase 4 nicety to the named fix for a
failing Gate 2 criterion. ADR-0022 is what the client reports; this ADR is
how the server reads. Schedule the slices together for Gate 2; keep the
schemas apart.

**This is not one mechanism with two implementations.** Matroska and MP4
share a session-scoped virtual-file *concept* and a keyframe map table; they
use **different** start paths with different residual costs and failure
modes (§3). Do not let “byte-offset start” collapse them in prose or code.

### Measured numbers (attached)

| Fact | Value | Source |
|---|---|---|
| NAS far-seek baseline (transcode, Matroska) | ~7.1 s | `far-seek-baseline-2026-08-01` |
| Cold purge matrix, HTTP Cluster start (Matroska) | land 537–797 ms (all under 3 s) | `far-seek-http-shim-2026-08-01` |
| Transcode sidx vs request (Matroska) | exact | same |
| Copy offset = request → Cluster PTS | far Δ +1.42 s → +0.083 s | same + copy-offset note |
| Library Matroska / MP4-family | 84.9% / **13.1%** | dogfood DB 24877 items |
| MP4 faststart (moov before mdat), n=300 | **17%** faststart / **83%** end-moov | `mp4-faststart-2026-08-01` |
| End-moov dogfood `-ss 60` (historical cost class) | ~19 seeks / ~12 s | `far-seek-cluster-spawn` |
| MP4 virtual faststart + `-ss` (warm spawn) | TC land ~1.3–1.6 s; sidx exact | `mp4-virtual-faststart-spawn-2026-08-01` |
| MP4 virtual faststart + `-ss` (**after purge**, mid-seek) | Grey’s TC **1453 ms**; Mincemeat TC **1740 ms**; sidx exact | same harness post-purge; mincemeat-cold JSON |

**MP4 evidence discipline.** The n=300 cold `-ss` comparison across largest
faststart vs end-moov files was **size-confounded and is not evidence** for
mechanism or Gate 2 — only the **17% / 83% faststart fraction** from that
sample counts (rules out header-prefetch-only for most MP4s). The historical
**19-seek / ~12 s** point is the cost *class* that motivated end-moov work;
it is not re-cited as the current cold wall on the titles above.

**Claim split (keep distinct):**

| Claim | Status |
|---|---|
| Matroska mechanism + Gate 2 cold (mid/far purge matrix) | Proven |
| MP4 mechanism (virtual faststart + map-PTS `-ss`, honest sidx; naive splice rejected) | Proven |
| MP4 Gate 2 cold mid-seek (`-ss 60`) on end-moov after purge | Proven on two titles (above) |
| MP4 Gate 2 cold **far** scrub on long titles | **Pending** implement-time dogfood (harness used mid-seek only) |

## Decision

### 1. Map shape (per container)

The map carries an explicit **container kind**. One undifferentiated byte
offset column is wrong: Matroska Cluster positions are not MP4 `stss`
sample offsets.

| Kind | Entry | Start path | Notes |
|---|---|---|---|
| `matroska` | Cluster absolute byte offset + Cluster PTS | Cluster splice; **no `-ss`** | FFmpeg reads from land Cluster |
| `mp4` | Key sample byte offset (`stss` / sample tables) + sample PTS | Virtual faststart; **keeps `-ss`** at map PTS | Map makes the file cheaply seekable; FFmpeg still seeks inside it |

Sparse enough for seek: one entry per video keyframe (or Cluster that starts
a keyframe), ordered by PTS. Exact on-disk encoding is §7.

### 2. Storage and scan cost

**Index-first.** After `find_stream_info`, read the container index (Matroska
Cues; MP4 `stss` / sample tables). That is a header-scale read for items with
a usable index — estimated ~84% of the library (Matroska-dominated; Cues
present on healthy remuxes).

**Packet walk** only where the index is missing or truncated — roughly the
remaining ~16%, estimated on the order of **~10 hours** wall across this
library at household NAS rates. One-time, **incremental on identity change**,
**resumable**, and **must never block a rescan** (same scheduling class as
subtitle extract under ADR-0013: probe/index finish; map builds in
background).

The truncation / usable-extent check is the same read as the DEF-8519 damage
signal. Record **usable extent** and map completeness from that one pass
when walking; do not schedule a second full demux for damage alone.

### 3. Session start — two mechanisms

Shared: session-scoped HTTP Range virtual file (§4); map snap; `landedMs`;
copy uses map PTS as `-output_ts_offset`; fallback §8. Not shared: how bytes
are laid out or whether FFmpeg is passed `-ss`.

#### 3a. Matroska — Cluster splice (no `-ss`)

1. Resolve greatest map entry with PTS ≤ request.
2. Virtual file: `[0, header_end)` from the real file, then
   `[land_Cluster, EOF)` spliced as one Matroska body. A Cluster is
   self-contained; that splice is a valid file.
3. **No `-ss`.** `-output_ts_offset` = map Cluster PTS.
4. Residual cost: header read + first Ranges into the land Cluster + encode.
   Failure mode: wrong Cluster byte → demux garbage (invalidation §6).

**Rejected:** `-ss` on the real path (~7 s baseline); pipe (non-seekable);
temp file per seek (copy of remainder).

Cold purge matrix: land 537–797 ms, transcode sidx exact.

#### 3b. MP4 — virtual faststart (keeps `-ss`)

Matroska’s splice does **not** apply. Sample offsets in `moov` are absolute
in the original file.

**Why `stco`/`co64` rewriting is load-bearing.** A naive splice
(`ftyp`+`moov`+`mdat[land:]` without rewriting chunk offsets) can still
emit a segment whose **`sidx` matches the requested land** via
`-output_ts_offset`, while audio fails (observed: AAC `channel element …
not allocated`). The metric looks right; the stream is broken. That is the
shortcut this ADR forbids. Rewriting chunk offsets is not optional polish;
it is what makes the virtual `moov` describe the bytes actually served.

**Locked path:**

1. Present **`[ftyp…][moov'][mdat]`** over HTTP Range. For end-moov sources,
   relocate `moov` after the prefix and rewrite every `stco`/`co64` by
   `+sizeof(moov)` (qt-faststart delta). Already-faststart sources: identity
   layout (no rewrite).
2. Serve `mdat` via Range onto the original `mdat` extent — no per-seek
   media temp copy.
3. Map snap; FFmpeg opens the URL with **`-ss` at the map PTS** (index seek
   into `mdat`, not a tail hunt for `moov`) plus `-output_ts_offset` as for
   Matroska copy/transcode rules.
4. Advertise **`landedMs`**.

**Asymmetry vs Matroska (explicit):** the map (plus virtual faststart) gets a
*valid seekable file* cheaply; FFmpeg **still performs its own `-ss` seek**
inside that file. Residual cost includes that seek and moov materialisation
(§3c). Failure modes differ: bad rewrite → wrong sample bytes / AAC-class
breakage; bad map PTS → wrong land with a coherent file; missing virtual
faststart on end-moov → return of the moov-hunt cost class.

**Rejected for MP4:** Matroska-style land splice without table rewrite;
pipe; temp faststart remux per seek.

A future no-`-ss` MP4 land (truncate sample tables to the snap) is out of
scope; map byte offsets are stored so that work needs no second index pass.

#### 3c. Virtual `moov'` lifetime (MP4)

The rewritten `moov` is **computed**, not a second copy of the media file.

**Settled: per session.** Build `moov'` when the virtual file is bound; drop
with the session. No `moov'` table or on-disk cache. Always matches the bytes
about to be served; pays rewrite cost on each session that needs end-moov
relocation (~1–3 s observed on dogfood sizes for a full rewrite). If Gate 2
cold dogfood shows that cost in the land path, revisit caching — any cache
must carry `content_id` like every other derived artifact (§6).

### 4. Scope of the virtual file

New internal surface: Nightjar serving byte ranges of media it opened for a
session to an FFmpeg subprocess.

- Bound to a **session** (and its current run): unguessable / session-scoped,
  not a general `/media/{id}` range server.
- Lifetime tied to the session (or run); not a public client API.
- Auth: same trust as other session media.
- **Two handlers, one concept:** Matroska Cluster splice vs MP4 virtual
  faststart. Same binding rules; different byte maps. Never feed MP4 into
  the Matroska splice handler.

**Mid-playback replace (settled).** *arr quality upgrades often rename over
the path while a session is open. An open FD that still holds the old inode
may keep playing old bytes until that producer exits — that is acceptable.
The dangerous case is the **next** bind (session start or post-seek run)
opening the new file with the old map's byte offsets.

At every virtual-file bind / FFmpeg input open: compute live `content_id`
from the path and compare it to the `content_id` the map (and this bind)
was keyed on. On mismatch: **do not** serve map offsets; fall through to
§8 (`-ss` on the real file + enqueue rebuild). Do not keep a bound virtual
file across an identity change. In-place truncate under a live FD (rare vs
atomic replace) surfaces as producer failure; treat that as §8, not as a
reason to keep splicing.

### 5. Advertise the snapped land

Reuse ADR-0020 **`landedMs`**. Do not invent a second snapped field.

### 6. Invalidation (ships with the map or the map does not ship)

A stale byte offset is worse than a missing one. Sonarr / Radarr replacements
are the normal case.

**One identity per media file**, referenced by every derived artifact:

- probe data
- extracted WebVTT (ADR-0013)
- extracted ASS (ADR-0019)
- usable extent
- keyframe map (this ADR)
- later trickplay

MP4 `moov'` is per-session (§3c), not a derived artifact.

**Media vs sidecars:** Bazarr writing an SRT updates `media_item_sidecars`
only — not a re-probe and not a new keyframe map.

**Identity:** `content_id` = **`size_bytes` + sha256(first 64 KiB) +
sha256(last 64 KiB)`** (fit prefix/suffix when smaller). **Computed at
scan and stored.** Day-to-day invalidation is a **stored comparison**
against derived stamps. Bind-time revalidation (§4) re-reads the windows
because a replace can land before the next scan. Rescan still uses
mtime/size to choose which paths to touch; fingerprint decides derived
validity. On mismatch: stale derived rows + **enqueue rebuild** (§8).
mtime/size alone is rejected for map invalidation.

The fingerprint is an identity check, not a security boundary. SHA-256 is
the digest in tree today; a lighter non-cryptographic hash is a fair later
argument if cost shows up — do not hand-roll one.

### 7. On-disk / on-wire shapes (Rule 4.9 — before any writer)

Locked in migration `007_content_identity_keyframe_map.sql`. Identity columns
live on `media_items` (not a separate identity table): `size_bytes` /
`mtime_ms` already exist; fingerprint stamps sit beside them.

**`content_id` string shape** (`nightjar_db::format_content_id`):

`{size_bytes}-{sha256_hex(first 64 KiB)}-{sha256_hex(last 64 KiB)}`

Lowercase hex digests; windows truncate when the file is smaller than 64 KiB.
Invalidation is string equality (`content_id_matches`).

**`media_items` additions:**

| Column | Role |
|---|---|
| `content_id` | live fingerprint, computed at scan |
| `probed_content_id` | identity probe columns were built under |
| `subtitle_content_id` | identity embedded extracts were built under |
| `usable_extent_ms` | DEF-8519 damage signal from map/index pass |
| `usable_extent_content_id` | identity that extent was measured under |
| `map_status` | `pending` \| `ready` \| `error` \| `unavailable` |
| `map_content_id` | identity the map entries were built under |

Sidecars stay on `media_item_sidecars` with their own mtime/size. A Bazarr SRT
does not change `content_id` and must not force re-probe or a new map.

**`keyframe_map_entries`:**

| Column | Role |
|---|---|
| `media_item_id` | FK |
| `content_id` | must match live identity to be used |
| `container_kind` | `matroska` \| `mp4` |
| `pts_ms` | title-absolute |
| `byte_offset` | kind-specific (§1) |

No MP4 `moov'` store (§3c settled per session).

Wire: clients keep **`landedMs`** (ADR-0020). No new session response field.

### 8. Fallback when there is no map

No ready map, or `content_id` mismatch: **today’s `-ss` on the real file**
(no virtual faststart / no Cluster splice). Do not fail the session.

**Rebuild trigger (required — writers slice, not deferred).** That fallback
**must enqueue a map rebuild** unless one is already pending or in flight.
The same enqueue runs when index upsert clears map rows on replace (mtime /
size path). *arr upgrades make replace continuous; without the trigger,
every upgraded title sits on the ~7 s path until a full rescan. Clearing
rows without rebuild is not a complete invalidation path.

## Consequences

- Gate 2 under-3s seek: Matroska claimable on measured cold matrix; MP4
  mechanism locked and mid-seek cold under 3 s on two end-moov titles after
  purge; MP4 far-scrub cold still implement-time dogfood.
- Two start paths, one map schema, one virtual-file binding model.
- Identity fingerprint is the common invalidation key. MP4 `moov'` is
  per-session, not a derived artifact.
- Full-title playlist cook remains dead for cold-seek latency.
- ADR-0022 stays a separate schema and slice.

## Out of scope

- Trickplay images
- Full-title playlists as a seek fix
- Restart-latency work beyond cold-open / land
- Library health flags (Rule 3.2 / parked)
- Changing the transcode segment grid (ADR-0008)
- MP4 no-`-ss` sample-table truncation land

## References (notes)

- `nightjar-meta/notes/far-seek-baseline-2026-08-01.md`
- `nightjar-meta/notes/far-seek-http-shim-2026-08-01.md`
- `nightjar-meta/notes/far-seek-cluster-spawn-2026-08-01.md`
- `nightjar-meta/notes/mp4-faststart-2026-08-01.md`
- `nightjar-meta/notes/mp4-virtual-faststart-spawn-2026-08-01.md`
- `nightjar-meta/notes/cut-rule-gating-2026-07-31.md` (elst / priming ~83 ms)
