# Iteration 3 — M4: the show-folder year is unreadable in a flat layout

## The mechanism

Two shipped helpers disagree about which directory is the show folder.

`show_folder_relpath` (ADR-0033 Q2) pops the filename, then pops season-named
directories, and what remains is the show folder. It is the rule the queue groups
by and the rule the migration retro-derives series rows with, *"so the two always
agree on the folder key."*

`year_from_show_folder` does not use it. It walks **exactly two parents**:

    pub fn year_from_show_folder(path: &str) -> Option<i32> {
        let show = Path::new(path).parent()?.parent()?.file_name()?.to_str()?;
        year_in_parens(show)
    }

For `Show (2001)/Season 01/ep.mkv` two parents up is the show folder and the two
agree. For a **flat** layout — `Show (2001)/ep.mkv` — two parents up is the
library root, and the year sitting in the folder name is never read. Confirmed
with the shipped predicates:

| path | `show_folder_relpath` | `year_from_show_folder` |
|---|---|---:|
| `Scrubs (2001)/Season 01/ep.mkv` | `Scrubs (2001)` | **2001** |
| `Scrubs (2001)/ep.mkv` | `Scrubs (2001)` | **None** |
| `Scrubs (2001)/Specials/ep.mkv` | `Scrubs (2001)` | 2001 |
| `Scrubs (2001)/Season 01/Disc 1/ep.mkv` | `Scrubs (2001)/Season 01/Disc 1` | None |

The year is on disk. The code cannot read it. Without a year the exact-title
collision has nothing to pin it, and the group goes below threshold —
`exact_title_collision_unpinned` at 0.72.

## Population, counted — and attributed with one variable

`tv.sonarr.plain` and `tv.flat.titled` render **the same filename** for the same
entities and differ only in the directory layout. That is the controlled pair the
oracle was built to provide.

| shape | layout | measured | correct | absent | rate |
|---|---|---:|---:|---:|---:|
| tv.sonarr.plain | `Show (Y)/Season NN/Show - SxxExx.mkv` | 3,594 | 3,594 | 0 | **100.0%** |
| tv.flat.titled | `Show (Y)/Show - SxxExx - Title.mkv` | 4,223 | 4,086 | **137** | 96.8% |

**All 137 absent slots in `tv.flat.titled` are `correct` in `tv.sonarr.plain`** —
same entity, same season, same episode. 21 distinct entities. One variable moved,
so the attribution is measured rather than argued.

`tv.flat` holds a further 457 absents and its folder also carries `(Year)`, so the
fix should reach them too — but `tv.flat`'s filename form differs from
`tv.sonarr.plain`'s, so those 457 are **plausible, not attributed**. The comparator
has no verdict for them: all 457 are `stalled` in `tv.sonarr.plain`.

So: **137 proven, up to 594 total.** Stated apart, because an estimate in a table
of measurements reads as a measurement.

Not reachable by this fix, and named so the population is not overclaimed:
`tv.scene` (395 — its folder is a scene release name with no year),
`tv.noyear` (143 — no year on disk by construction), `tv.root` (457 — no folder
at all), `movie.noyear` (705 — no year anywhere). Those are M5 proper: the year
does not exist, so no reading rule recovers it.

## The convention it depends on, and what it does without it

**It depends on `(YYYY)` in the show folder name** — `year_in_parens`, the shipped
predicate. A folder written `Show 2001` or `Show.2001` yields nothing, and this
change does not alter that. So the honest scope is the parenthesised form, which
is what all five oracle shapes with a folder year use, and what Sonarr writes.

**It does not fix a show folder deeper than the season directory** —
`Show (2001)/Season 01/Disc 1/ep.mkv`. `show_folder_relpath` stops at `Disc 1`
because `Disc 1` is not a season directory, so both helpers agree and both are
wrong. No oracle shape generates it, and it is a different mechanism.

## Dogfood regression risk

The dogfood library is `Show (Year)/Season N/file` throughout, where the
two-parent walk already lands on the show folder. `show_folder_relpath` returns
the same folder for that layout, so the derived year is **the same value by
construction** and the pair should be identical. `populations.py` reports *"M4
show-folder year present but unreadable — 0 of 9,568 (0.00%)"*.

The real risk is the opposite direction: supplying a year where none was supplied
before could let a year pin **reject** a correct candidate whose provider year
disagrees with the folder's. `tv.sonarr` and `tv.sonarr.plain` already receive
this year and both sit at 100.0%, so a correct folder year is not harmful for
these entities — but that is evidence about these entities, not a proof.

Not a wins argument. Purely: what could break.

## Prediction, written before running

| shape | after iter 2 | predicted | why |
|---|---:|---:|---|
| **tv.flat.titled** | 96.8% | **100.0%** | all 137 absents are correct in the controlled comparator |
| **tv.flat** | 88.4% | **95–100%** | folder carries a year; form differs, so not attributed |
| tv.root | 88.4% | **unchanged** | no folder at all, so nothing to read |
| tv.scene, tv.noyear | — | **unchanged** | no year in the folder |
| movie.* | — | **unchanged** | movies use `year_from_path`, untouched |
| tv.sonarr, tv.sonarr.plain, tv.partial, tv.single, tv.twoseason | 100.0% | **100.0%** | already at the ceiling; the derived year must be the same value |
| overall correct% | 83.0% | **84–85%** | |

The sharpest test is `tv.sonarr.plain` staying at exactly 100.0% with 3,594
correct. It receives a show-folder year today by the two-parent walk. If the new
derivation is right, that number cannot move at all — and if it moves, the two
rules disagree somewhere I have not looked.

---

## The change

`year_from_show_folder_at(path, library_root)` reads the year off whatever
`show_folder_relpath` calls the show folder, instead of walking a fixed two
parents. `year_from_show_folder(path)` stays as `…_at(path, "")`, so one
implementation serves both entry points and the harness keeps compiling.
`series_library_year` takes the library root and passes it through; the drain has
one in `library_path0`, and the three measurement binaries have none and pass
`""`.

One production call site changed. `nightjar-core` and `nightjar-scanner` remain
byte-identical to `origin/main`.

Two shipped tests gained the new argument, with the real library root the paths
already implied (`/Volumes/media/TV Shows`) rather than a placeholder — the
assertions themselves are unchanged. Nothing was weakened. One test added:
`series_library_year_reads_a_flat_show_folder`, which the two-parent walk fails.
Metadata went 257 → 258 tests.

## Measured — four instruments

### 1. The oracle

| shape | correct b | correct a | wrong b | wrong a | rate b | rate a |
|---|---:|---:|---:|---:|---:|---:|
| **tv.flat.titled** | 4,086 | **4,311** | 0 | 0 | 96.8% | **100.0%** |
| **tv.flat** | 3,547 | **4,083** | **8** | **0** | 88.4% | **100.0%** |
| every other shape | — | — | — | — | unchanged | unchanged |

**Two shapes moved — exactly the two with a year in a flat show folder. Fifteen
byte-identical.**

| verdict | before | after | delta |
|---|---:|---:|---:|
| correct | 38,818 | 39,579 | **+761** |
| wrong.entity | 28 | 20 | **−8** |
| absent | 7,941 | 7,347 | **−594** |
| stalled | 21,195 | 21,036 | −159 |
| **correct%** | **83.0%** | **84.3%** | **+1.34 pt** |

**Four transitions, none of them a regression:**

    absent       -> correct    590
    stalled      -> correct    163
    wrong.entity -> correct      8
    absent       -> stalled      4

Noise floor **0 of 67,982**.

**`absent` fell by exactly 594** — the 137 attributed plus the 457 plausible, the
whole counted population and nothing outside it. That is the population count
confirming itself: had the classifier over-counted, the number would have come in
under 594; had it missed a path, over.

### The 8 wrong binds were a bonus, and they are explained

`tv.flat`'s 8 `wrong.entity` rows — Queer as Folk → Queer as Folk (2022) — went to
`correct`. The revival collision needs a year to break the tie, the year was in
the folder all along, and nothing could read it. So M4 was not only costing
absents; it was causing a share of the revival wrong-bind class too.

The same 8 rows persist in `tv.root` (still 88.4%) and `tv.scene` (89.5%),
which is the control on that claim: neither has a readable year in a folder, so
neither is fixed, and the mechanism is the year rather than the change touching
the collision logic.

### The sharpest test

`tv.sonarr.plain` held at **exactly 3,594 correct, 100.0%**, and `tv.sonarr`,
`tv.partial`, `tv.single` and `tv.twoseason` likewise did not move by a row.
Those shapes already received a show-folder year by the two-parent walk. If the
new derivation disagreed anywhere, the number would have moved. It did not, on
16,978 rows.

### 2. The parser sweep

74,624 names, 0 gains, 0 regressions — insensitive by construction,
`nightjar-core` byte-identical to `origin/main`.

### 3. The parser corpus

**71.0%** (524 of 738) — unchanged.

### 4. The dogfood strict pair

    control   sha256 df06d521…   DONE groups=3220 ready=24953 unmatched=51 errors=0 requests=0
    treatment sha256 92ca0a6c…   DONE groups=3220 ready=24953 unmatched=51 errors=0 requests=0
    cache 8185 before, 8185 after

Identical, as predicted: the library is `Show (Year)/Season N/file` throughout,
where the old walk and the show-folder rule name the same directory, so the
derived year is the same value.

### Shipped tests

741 pass, 0 fail, 3 ignored. One added.

## The prediction

| | predicted | measured |
|---|---|---|
| tv.flat.titled | 100.0% | **100.0%** |
| tv.flat | 95–100% | **100.0%** |
| tv.root, tv.scene, tv.noyear, movie.* | unchanged | **unchanged** |
| tv.sonarr.plain and the other 100% shapes | exactly 100.0% | **exactly 100.0%** |
| overall correct% | 84–85% | **84.3%** |

Every line landed. The `wrong.entity −8` was **not** predicted — I expected the
year to fix absents and did not think through that it also breaks a revival tie.
A prediction that under-claims is still a miss, and the finding is that M4 was
feeding the wrong-bind class as well as the absent class.

## Verdict — KEEP

Oracle up on both targeted shapes and on the total, no regression in 67,982 rows,
a wrong-bind class partly closed, the other three instruments clean or provably
insensitive, tests green with one added, noise floor zero.
