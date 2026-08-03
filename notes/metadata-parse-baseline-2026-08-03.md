# Filename parse baseline — dogfood (2026-08-03)

Phase 3 block 1 measurement. Read-only. No parser changes.

**Corpus:** `~/nightjar-data/nightjar.db`, 24,940 `media_items` rows.
**Parser:** `nightjar_core::parse_filename` (Phase 1), run on each stored
path's basename. No TMDB/TVDB calls.
**Repro:** `DB=~/nightjar-data/nightjar.db cargo run -p nightjar-metadata
--bin metadata-parse-measure` (JSON on stdout).

## Confident / unusable

| Outcome | Count | % of 24,940 |
|---|---:|---:|
| Confident movie (title + year) | 1,760 | 7.1% |
| Confident episode (show title + season + episode) | 23,076 | 92.5% |
| Nothing usable | 104 | 0.42% |

Definitions used here:

- Confident movie: `MediaKind::Movie`, `year.is_some()`, non-empty title.
- Confident episode: `MediaKind::Episode`, season and episode both `Some`,
  non-empty title.
- The Phase 1 parser always returns Movie or Episode; it never emits
  `unknown`. Unusable is therefore almost entirely Movie-without-year
  (including TV specials that fail the `season > 0 && episode > 0` guard).

### By library

| Library | Items | Confident movie | Confident episode | Unusable |
|---|---:|---:|---:|---:|
| Movies | 1,773 | 1,749 | 0 | 24 |
| Shows | 23,099 | 11 | 23,075 | 13 |
| Test Data | 65 | 0 | 1 | 64 |
| DV2 | 3 | 0 | 0 | 3 |

Movies+Shows only (24,872): 1,760 / 23,075 / 37 unusable. The 24 Movies
unusable rows are all `Patterns_Of_Nature_*` HDR/DoVi kit files living
under the Movies root, not theatrical titles.

## Unusable buckets (n = 104)

| Bucket | Count | What it is on this library |
|---|---:|---|
| no_year | 56 | Movie-shaped parse, no 1900–2100 year token. Dominated by Patterns_Of_Nature kit files (51) plus a few real TV titles without SxxExx/year. |
| codec_fixture_name | 34 | `testdata/files` stems (`h264_*`, `hevc_*`, `av1_*`, …). |
| season_or_episode_zero | 6 | S00 / E00 / x00 / 0xNN — parser requires season and episode > 0. |
| ambiguous_separator | 4 | Opaque single-token stems (`Movie.mp4`, `Show.mp4`, `PAM_ANN.m4v`, `P5_Dolby_Amaze.mkv`). |
| special_token | 3 | "Special" / non-numeric special naming without a usable SxxExx. |
| extra_or_sample | 1 | Trailer/demo filename (`P4_LG_Dolby_Trailer_4K_Demo.mkv`). |
| anime_absolute_number | 0 | No unusable `Show - NN` absolute-number files in this DB. |
| multi_episode_file | 0 | Multi-episode names still match the first SxxExx/NxNN (see below). |
| non_UTF8_basename | 0 | Every stored basename was valid UTF-8. |

## Top 20 failure shapes

| # | Count | Shape | Example |
|---:|---:|---|---|
| 1 | 17 | `Patterns_Of_Nature_DoVi_<profile>_<res>_<codec>.mp4` [no_year] | `Patterns_Of_Nature_DoVi_24_P5_FHD_HEVC-4mbps_DD+JOC-768kbps_iOS.mp4` |
| 2 | 17 | `Patterns_Of_Nature_HDR10_<profile>_<res>_<codec>.mp4` [no_year] | `Patterns_Of_Nature_HDR10-P8.1_FHD_24_H265-4Mbps_DD+JOC-768Kbps.mp4` |
| 3 | 17 | `Patterns_Of_Nature_HLG_<profile>_<res>_<codec>.mp4` [no_year] | `Patterns_Of_Nature_HLG-P8.4_FHD_24_H265-4Mbps_DD+JOC-768Kbps.mp4` |
| 4 | 6 | `h264_aac_<codec_fixture>.mkv` [codec_fixture] | `h264_aac_ass_mkv.mkv` |
| 5 | 5 | `h264_aac_<codec_fixture>.mp4` [codec_fixture] | `h264_aac_32k_mp4.mp4` |
| 6 | 3 | `<show> - NNx00 - <title>.mkv` [season_or_episode_zero] | `Top Gear - 22x00 - Special Patagonia Part One.mkv` |
| 7 | 3 | `<title_words_no_year>.mkv` [no_year] | `Red Dwarf S09 Back to Earth DC 1080p BluRay HEVC x265 5.1 BONE.mkv` |
| 8 | 2 | `<opaque_token>.mp4` [ambiguous_separator] | `Movie.mp4` |
| 9 | 2 | `<show> - <title> Special.mkv` [special_token] | `Top Gear - Polar Special.mkv` |
| 10 | 2 | `<show> - <title_no_SxxExx_no_year>.mkv` [no_year] | `Top Gear Apocalypse - 1080p.mkv` |
| 11 | 2 | `hevc_dv_<codec_fixture>.mkv` [codec_fixture] | `hevc_dv_p81_pair.mkv` |
| 12 | 2 | `hevc_dv_<codec_fixture>.mp4` [codec_fixture] | `hevc_dv_p81_pair.mp4` |
| 13 | 1 | trailer/demo [extra_or_sample] | `P4_LG_Dolby_Trailer_4K_Demo.mkv` |
| 14 | 1 | `<opaque_token>.m4v` [ambiguous_separator] | `PAM_ANN.m4v` |
| 15 | 1 | `<opaque_token>.mkv` [ambiguous_separator] | `P5_Dolby_Amaze.mkv` |
| 16 | 1 | `<show> - 0xNN - <title>.mp4` [season_or_episode_zero] | `True Blood - 0x14 - A Farewell to Bon Temps - SDTV.mp4` |
| 17 | 1 | `<show> - NxSpecial - <title>.mkv` [special_token] | `Sherlock - 3xSpecial 9 - The Abominable Bride.mkv` |
| 18 | 1 | `<show> - S00Exx - <title>.mkv` [season_or_episode_zero] | `Family Guy - S00E07 - Family Guy (Pilot).mkv` |
| 19 | 1 | `<show> - SxxE00 - <title>.mkv` [season_or_episode_zero] | `Star Trek - S01E00 The Cage.mkv` |
| 20 | 1 | `av1_aac_<codec_fixture>.mp4` [codec_fixture] | `av1_aac_mp4.mp4` |

Remaining unusable mass below the top 20 is more single-count codec
fixtures under Test Data / DV2 (same `codec_fixture_name` bucket).

## Shows-library unusable (the product-relevant 13)

| Bucket | File |
|---|---|
| season_or_episode_zero | `Family Guy - S00E07 - Family Guy (Pilot).mkv` |
| season_or_episode_zero | `Star Trek - S01E00 The Cage.mkv` |
| season_or_episode_zero | `Top Gear - 16x00 -  The three wise men christmas special - 720p.mkv` |
| season_or_episode_zero | `Top Gear - 22x00 - Special Patagonia Part One.mkv` |
| season_or_episode_zero | `Top Gear - 22x00 - Special Patagonia Part Two.mkv` |
| season_or_episode_zero | `True Blood - 0x14 - A Farewell to Bon Temps - SDTV.mp4` |
| special_token | `Sherlock - 3xSpecial 9 - The Abominable Bride.mkv` |
| special_token | `Top Gear - Polar Special.mkv` |
| special_token | `Top Gear - The Great Adventures 3 - Romania Special - 1080p.mkv` |
| no_year | `Red Dwarf S09 Back to Earth DC 1080p BluRay HEVC x265 5.1 BONE.mkv` |
| no_year | `Top Gear -The Worst Car In The History Of The World - 720p .mkv` |
| no_year | `Top Gear Apocalypse - 1080p.mkv` |
| ambiguous_separator | `PAM_ANN.m4v` |

## Caveats (not counted as unusable)

- **Multi-episode files (33)** parse as a single confident episode (first
  `NxNN` / `SxxExx` wins). Examples: `30 Rock - 5x20-21 - 100 - Bluray-1080p.mkv`,
  `Abbott Elementary - 3x01-02 - Career Day - WEBDL-1080p.mkv`. Identity
  cardinality for these is ADR-0025 §2; the filename parser does not emit
  two episode numbers.
- **Anime absolute numbering:** 0 unusable hits. Dogfood anime paths that
  are present use SxxExx-style names and land in the confident-episode
  bucket; this measure does not score match quality.
- **Non-UTF8:** 0. SQLite `path` TEXT is already UTF-8; a filesystem-only
  non-UTF8 name would have been lossy at scan time and is not visible here.

## Distinct show titles (confident episodes)

Corpus: the 23,076 confident episode parses above. Show key =
`parse_filename(...).title` after the parser's own `clean_title`
(`.`/`_` → space, collapse spaces, trim `-`). No case-fold, no
orthography fold, no year-paren strip.

| Metric | Value |
|---|---:|
| Distinct parser titles | **725** |
| Largest show | The Simpsons — **800** episodes |
| Median episodes / show | **12** |
| Shows with a single episode | **7** |

Single-episode titles: `Bleach`, `INVINCIBLE (2021)`, `P81 GlassBlowing2 38`,
`President Curtis`, `Richard Hammond's BIG`,
`Stuart Fails to Save the Universe`, `The Pendragon Cycle`.

Largest (for scale): The Simpsons 800, Grey's Anatomy 465, Family Guy 456,
South Park 330, Supernatural 311, Chicago Fire 295, Blue Bloods 293,
Will & Grace 246, Bones 245, JAG 226.

Mode of the distribution is season-shaped: 110 shows have exactly 8
episodes; 69 have 6; 53 have 10.

### Near-duplicate parser titles

Loose key = alphanumeric + lowercased (punctuation/spacing/case ignored).
**3 groups** (6 titles) among the 725:

| Loose key | Titles (episode counts) |
|---|---|
| `theinspiredunemployedimpracticaljokers` | `The Inspired Unemployed (Impractical) Jokers` (24), `the inspired unemployed impractical jokers` (2) |
| `starwarsmaulshadowlord` | `Star Wars - Maul - Shadow Lord` (2), `Star Wars - Maul – Shadow Lord` (8) — ASCII hyphen vs en-dash |
| `invincible2021` | `INVINCIBLE (2021)` (1), `Invincible (2021)` (7) — case only |

### Is 725 real, or a parser-grouping artifact?

**Mostly real for the parser; slightly high vs the matcher soft key.**

`parse_filename`'s `clean_title` only unifies `.`/`_` spacing. It does **not**
repeat matcher work in `clean_show_title` / `fold_title_orthography`
(case-fold is absent; `&`↔`and`, apostrophe strip, en-dash→space, year-paren
strip are matcher-side).

Against the same 23,076 titles:

| Grouping | Distinct |
|---|---:|
| Parser `title` (this section) | 725 |
| `fold_title_orthography` + lower (matcher fold, no year strip) | 723 |
| `clean_show_title` (matcher soft key) | **720** |

The five extra parser splits that `clean_show_title` merges are year-paren
twins, not spelling noise:

- `Invincible` / `Invincible (2021)`
- `All's Fair` / `All's Fair (2025)`
- `Avatar - The Last Airbender` / `Avatar - The Last Airbender (2024)`
- `Scrubs` / `Scrubs (2026)`
- `Hacks` / `Hacks (2021)`

So: treat **725** as the Phase 1 parse distinct-show count. Do not treat it
as the Visible / queue soft-key count — that is **720** after matcher
cleaning. The three near-dup groups above are real filename inconsistency;
only the Maul en-dash pair is also folded by matcher orthography (with
case-fold). `INVINCIBLE` vs `Invincible` stays split under `clean_show_title`
because that helper does not case-fold.

Repro: `cargo run -p nightjar-metadata --bin metadata-show-distinct-measure`.

## Multi-episode files (first-NxNN-wins)

All **33** confident-episode files whose basename carries a discarded
episode range. Pattern on this library: **100% `NxMM-NN`** (and one
`NxMM-NN-NN`). No `SxxExxEyy`, no `SxxExx-Eyy` / `SxxExx-yy`.

| Span | Count |
|---|---:|
| 2 episodes | 32 |
| 3 episodes | **1** |

The only span &gt; 2: `Red Dwarf - 8x01-02-03 - Back in the Red - Bluray-1080p.mkv`
(parses as S8E1; discards 2 and 3).

| # | Span | File |
|---:|---:|---|
| 1 | 2 | `30 Rock - 5x20-21 - 100 - Bluray-1080p.mkv` |
| 2 | 2 | `30 Rock - 6x06-07 - Hey, Baby, What's Wrong! - Bluray-1080p.mkv` |
| 3 | 2 | `30 Rock - 7x12-13 - Hogcock! + Last Lunch - Bluray-1080p.mkv` |
| 4 | 2 | `Abbott Elementary - 3x01-02 - Career Day - WEBDL-1080p.mkv` |
| 5 | 2 | `Avatar - The Last Airbender - 2x12-13 - The Serpent's Pass + The Drill - Bluray-1080p.mkv` |
| 6 | 2 | `Avatar - The Last Airbender - 2x19-20 - The Guru + The Crossroads of Destiny - Bluray-1080p.mkv` |
| 7 | 2 | `Bones - 4x01-02 - Yanks in the U.K. - Bluray-1080p.mp4` |
| 8 | 2 | `Charmed - 5x01-02 - A Witch's Tail - SDTV.mkv` |
| 9 | 2 | `Charmed - 5x22-23 - Oh My Goddess! - SDTV.mkv` |
| 10 | 2 | `Elementary - 1x23-24 - The Woman + Heroine - HDTV-720p.mkv` |
| 11 | 2 | `Grey's Anatomy - 11x22-23 - She's Leaving Home - WEBRip-1080p.mp4` |
| 12 | 2 | `JAG - 1x01-02 - A New Life - SDTV.avi` |
| 13 | **3** | `Red Dwarf - 8x01-02-03 - Back in the Red - Bluray-1080p.mkv` |
| 14 | 2 | `Red Dwarf - 8x06-07 - Pete - Bluray-1080p.mkv` |
| 15 | 2 | `Star Trek - Deep Space Nine - 1x01-02 - Emissary - WEBDL-480p.mkv` |
| 16 | 2 | `Star Trek - Deep Space Nine - 4x01-02 - The Way of the Warrior - HDTV-1080p.mkv` |
| 17 | 2 | `Star Trek - Deep Space Nine - 7x25-26 - What You Leave Behind - HDTV-1080p.mkv` |
| 18 | 2 | `Star Trek - Enterprise - 1x01-02 - Broken Bow - HDTV-1080p.mkv` |
| 19 | 2 | `Star Trek - The Next Generation - 1x01-02 - Encounter at Farpoint - HDTV-1080p.mkv` |
| 20 | 2 | `Star Trek - The Next Generation - 7x25-26 - All Good Things. - HDTV-1080p.m4v` |
| 21 | 2 | `Star Trek - Voyager - 1x01-02 - Caretaker - Bluray-1080p.mkv` |
| 22 | 2 | `Star Trek - Voyager - 5x15-16 - Dark Frontier - Bluray-1080p.mkv` |
| 23 | 2 | `Star Trek - Voyager - 7x09-10 - Flesh and Blood - Bluray-1080p.mkv` |
| 24 | 2 | `Star Trek - Voyager - 7x25-26 - Endgame - Bluray-1080p.mkv` |
| 25 | 2 | `Star Wars Rebels - 4x15-16 - Family Reunion – and Farewell - Bluray-1080p.mkv` |
| 26 | 2 | `Stargate SG-1 - 1x01-02 - Children of the Gods - Bluray-1080p.mp4` |
| 27 | 2 | `The Expanse - 1x09-10 - Critical Mass + Leviathan Wakes - HDTV-1080p.mkv` |
| 28 | 2 | `The Expanse - 2x01-02 - Safe + Doors & Corners - HDTV-720p.mkv` |
| 29 | 2 | `The Expanse - 3x12-13 - Congregation + Abaddon's Gate - HDTV-720p.mkv` |
| 30 | 2 | `The Mentalist - 3x23-24 - Strawberries and Cream - HDTV-720p.mkv` |
| 31 | 2 | `The Mentalist - 7x12-13 - Brown Shag Carpet + White Orchids - HDTV-720p.mkv` |
| 32 | 2 | `The Simpsons - 28x12-13 - The Great Phatsby - WEBDL-1080p.mkv` |
| 33 | 2 | `The X-Files - 9x19-20 - The Truth - HDTV-1080p.mkv` |

## Gate 3 denominator (follow-up, 2026-08-03)

Match work is **2,485 decisions** (725 show titles + 1,760 movies), not
24,940 file rows. That shrinks Block 1 review surface by an order of
magnitude and makes the 30-second manual fix flow a viable fallback for
matcher misses.

Two accuracy numbers; they diverge because a wrong show match takes its
whole episode list with it (The Simpsons = 800 items from one decision):

| Number | Denominator | Meaning |
|---|---:|---|
| Decision accuracy | 2,485 matches | What you can measure and fix per soft key |
| Item accuracy | 24,836 items under those matches (23,076 ep + 1,760 movie) | What the user experiences |

**Proposed Gate 3 wording:** criterion = **item accuracy ≥95%** on the
team's real libraries; report **decision accuracy** alongside. Long tail:
median 12 eps/show and 7 singles mean most decisions are light; a handful
are enormous. Order manual review by episode count — the 20 largest shows
cover a disproportionate share and take minutes to eyeball.

95% of 2,485 decisions ≈ 124 wrong decisions allowed under a
decision-accuracy reading; that is not the same bar as 95% of items.

No ADR for this restatement yet. The expensive open ADR remains the
watch-state key question (prior message).

### Smaller follow-ups (not done in this note)

1. **Near-duplicates → matcher soft key, not parser.** Case + en-dash are
   the same class as the five year-paren twins `clean_show_title` already
   collapses. Keep on-disk parse honest; fold in the soft key. Maul
   hyphen/en-dash will recur on any dash-containing title.
2. **Testdata contamination.** Confirmed 2026-08-03: `P81 GlassBlowing2 38`
   lives only under library `Test Data` →
   `/Users/.../nightjar/testdata/files/...` — a **separate library root**,
   not under Movies/Shows. Dogfood DB indexes it as library_id 1. Measures
   filter it downstream (`EXCLUDE_TESTDATA=1` /
   `MEASURE_EXCLUDE_LIBRARY_NAMES` default `Test Data,DV,DV2`); this parse
   baseline did **not** apply that filter, so the fixture entered the 725.
   Product libraries alone: **724** distinct show titles, **1,760**
   confident movies, **2,484** decisions.
3. **`Patterns_Of_Nature_*` — decided: move, not asterisk.** Nested kit tree
   lived at `Movies/dolby-vision-browser-kit/` (24 indexed rows; 1.4% of
   1,760 movie decisions if left as permanent noise — a quarter of Gate 3's
   5% error budget). Moved 2026-08-03 to
   `/Volumes/media/dolby-vision-browser-kit` (DV library root, already in
   the measure-exclude set). Dogfood DB still has the old Movies paths until
   Movies + DV are rescanned.
4. **Multi-episode.** All 33 are one form (`NxMM-NN`); 32 are two-episode
   spans. Bounded parser fix inside the matcher slice, not a design problem:
   parse the range, then decide file→multiple items vs one item with a span.
   If a file can produce more than one join row, that is an on-disk shape
   question (**Rule 4.9**) before writers land.

## Block 1 close-out standing order (2026-08-03)

Durable numbers from this measure (product libraries, exclude
`Test Data,DV,DV2`; Patterns moved out of Movies):

| Figure | Value |
|---|---:|
| Match decisions | **2,484** (724 shows + 1,760 movies) |
| Items under those matches | 24,835 (23,075 ep + 1,760 movie) |
| Gate 3 match criterion | **item** accuracy ≥95%; report decision accuracy alongside |
| Parse: confident movie / episode / unusable (full DB) | 1,760 / 23,076 / 104 |

**Watch key (settled):** ADR-0025 — `item_key`, not `media_items.id` /
`content_id`. **Path-keyed watch state is permanent, not transitional:**
anything that stays `unmatched` (below 0.80, no results, home video) keeps
`path:{library_id}:{relpath}` for its lifetime (ADR-0026); that set loses
history on library remove-and-re-add. Gate 3 remove-and-re-add tests
provider-keyed resume only. Below-floor / fragile fraction ~4.5% on the
calibrated sample (full-library measure owns the real number).

**Before matcher writers / watch-state writers:**

1. **API key / attribution** — ADR-0031 **accepted** (2026-08-03). Next
   prerequisite before the 50-show TMDB coverage sample (§7): implement
   §4 override path (product secrets / env / embedded + named refuse).
2. **ADR-0027 written** before any artwork cache keys are written. Second
   reserved-ADR-with-a-dependent (with 0028's handoff).

**Then** the matcher itself, with near-dup soft-key fold (case + en-dash,
same class as year-paren twins) and multi-episode range parse as
implementation detail inside it — not separate Block 1 paper.

### ADR-0030 items still queued

From `notes/migration-012-dogfood-2026-08-03.md` and ADR-0030 consequences:
dry-run on a VACUUM copy passed (24,940/8,583, zero unresolved). Still open:

- Live dogfood migration (not the disposable copy)
- Gate 3 remount / Docker bind-path repoint verification against live library
- Implementing-slice leftovers named in ADR-0030: async path PATCH + dry-run
  retain guard; OpenAPI note on `MediaItem.path`; `skipped_outside_root` /
  `paths_unresolved` / `deferred_remove` visible to a client or `nightjar
  doctor` (server knows; household UI does not yet)

SMB Movies walk cost and full-walk-before-`delete_missing` noted separately:
`notes/scan-walk-smb-movies-2026-08-03.md` (not Block 1 work).
