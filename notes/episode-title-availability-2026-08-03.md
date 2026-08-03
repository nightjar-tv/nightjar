# Episode title availability (after SxxExx / NxNN)

**Date:** 2026-08-03  
**Harness:** `metadata-episode-title-measure`  
**Raw JSON:** `notes/episode-title-availability-2026-08-03.json`  
**Corpus:** `~/nightjar-data/nightjar.db`, same confident-episode definition as
the parse baseline (23,076).

## Question

Of confident episode parses, how many basenames carry an episode title
**after** the SxxExx / NxNN token? Enough signal for an episode-title
disambiguation tier? Measure only — no product path built.

## Method

- Confident = parser `kind == episode` with season, episode, non-empty show
  title (same bar as parse baseline).
- Title component = text after the matched SxxExx / NxNN token, with release
  junk stripped (`bluray` / `webdl` / `1080p` / …). Junk-only after-token
  counts as **without**.
- Placeholders: `Episode N` / `Episode NN`, `Ep N`, show title repeated, bare
  number (digits / `.` / `-` only).

## Results

| | Count | % of 23,076 |
|---|---:|---:|
| Confident episodes | 23,076 | 100% |
| With title component (after junk strip) | **23,060** | 99.93% |
| Without title component | **16** | 0.07% |
| Of those with a component: placeholders | **1,228** | 5.3% of with |
| Of those with a component: distinctive | **21,832** | 94.7% of with |

### Placeholder breakdown

| Kind | Count |
|---|---:|
| `Episode N` / `Episode NN` | 1,158 |
| `Ep N` | 0 |
| Show title repeated | 16 |
| Bare number | 54 |

Genuine distinctive titles after removing placeholders: **21,832**.

## Examples

### Distinctive (20)

- `1883 - 1x02 - Behind Us, a Cliff - Bluray-1080p.mkv` → `Behind Us, a Cliff`
- `1883 - 1x03 - River - Bluray-1080p.mkv` → `River`
- `1883 - 1x04 - The Crossing - Bluray-1080p.mkv` → `The Crossing`
- `1883 - 1x05 - The Fangs of Freedom - Bluray-1080p.mkv` → `The Fangs of Freedom`
- `1883 - 1x06 - Boring the Devil - Bluray-1080p.mkv` → `Boring the Devil`
- `1883 - 1x07 - Lightning Yellow Hair - Bluray-1080p.mkv` → `Lightning Yellow Hair`
- `1883 - 1x08 - The Weep of Surrender - Bluray-1080p.mkv` → `The Weep of Surrender`
- `1883 - 1x09 - Racing Clouds - Bluray-1080p.mkv` → `Racing Clouds`
- `1883 - 1x10 - This Is Not Your Heaven - Bluray-1080p.mkv` → `This Is Not Your Heaven`
- `1899 - 1x01 - The Ship - WEBDL-1080p.mkv` → `The Ship`
- `1899 - 1x02 - The Boy - WEBDL-1080p.mkv` → `The Boy`
- `1899 - 1x03 - The Fog - WEBDL-1080p.mkv` → `The Fog`
- `1899 - 1x04 - The Fight - WEBDL-1080p.mkv` → `The Fight`
- `1899 - 1x05 - The Calling - WEBDL-1080p.mkv` → `The Calling`
- `1899 - 1x06 - The Pyramid - WEBDL-1080p.mkv` → `The Pyramid`
- `1899 - 1x07 - The Storm - WEBDL-1080p.mkv` → `The Storm`
- `1899 - 1x08 - The Key - WEBDL-1080p.mkv` → `The Key`
- `1923 - 1x02 - Nature's Empty Throne - Bluray-1080p.mkv` → `Nature's Empty Throne`
- `1923 - 1x03 - The War Has Come Home - Bluray-1080p.mkv` → `The War Has Come Home`
- `1923 - 1x04 - War and the Turquoise Tide - Bluray-1080p.mkv` → `War and the Turquoise Tide`

### Placeholders (10)

- `1883 - 1x01 - 1883 - Bluray-1080p.mkv` → `1883` (show repeated)
- `1923 - 1x01 - 1923 - Bluray-1080p.mkv` → `1923` (show repeated)
- `30 Rock - 2x10 - Episode 210 - Bluray-1080p.mkv` → `Episode 210`
- `30 Rock - 5x20-21 - 100 - Bluray-1080p.mkv` → `21 - 100` (bare / range leftover)
- `9-1-1 - 2x02 - 7.1 - WEBDL-1080p.mkv` → `7.1` (bare-number heuristic; may be a real title)
- `A Discovery of Witches - 1x01 - Episode 1 - Bluray-1080p.mkv` → `Episode 1`
- `A Discovery of Witches - 1x02 - Episode 2 - Bluray-1080p.mkv` → `Episode 2`
- `A Discovery of Witches - 1x03 - Episode 3 - Bluray-1080p.mkv` → `Episode 3`
- `A Discovery of Witches - 1x04 - Episode 4 - Bluray-1080p.mkv` → `Episode 4`
- `A Discovery of Witches - 1x05 - Episode 5 - Bluray-1080p.mkv` → `Episode 5`

## Bare-number eyeball (54)

Re-listed every bare-number classification from the dogfood DB. **Almost all
are real episode titles**, not audio-layout tokens or placeholders:

- Chernobyl `1-23-45`, Star Trek TNG `11001001`, Voyager `11-59`, Battlestar
  Galactica `33`, Ted Lasso `4-5-1`, The Crown `48-1`, 9-1-1 `7.1`, Promised
  Neverland numeric codes (`121045`, …), Squid Game player numbers, etc.
- One clear parse leftover: `30 Rock - 5x20-21 - 100` → `21 - 100` (range
  tail after first-NxNN-wins).
- Show-title-as-number pilots (`1883`, `1923`) overlap the "show repeated"
  class when the show title is itself digits.

**Conclusion:** the bare-number heuristic over-counts placeholders. The
21,832 distinctive figure is a **lower** bound — most of the 54 belong with
distinctive titles. Do not treat "bare number" as junk when designing an
episode-title tie-break; numeric titles are real signal (and are exactly
the case where year/ID may still be needed when TMDB also uses a number).

## Implication (for the human, not a build)

Availability is not the blocker: after junk strip, nearly every confident
episode carries after-token text, and the large majority is distinctive
(even more once bare-number false placeholders are folded back in). A
collision tier that fires only on multi-exact title ties can use that
signal; year and explicit ID remain complementary for cases like Top Gear
where S01E01 is `Episode 1` on every candidate.

