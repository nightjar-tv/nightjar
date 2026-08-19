# Iteration 1 — M6: every show in a shared root becomes one group

## The mechanism

`status_query_groups` scopes TV groups by folder (ADR-0033 Q2). The key is
`(library_id, show_folder_relpath(path, library_path))`, and that is deliberate:
two folders that fold to the same matcher key — `Shameless (US)` and
`Shameless (UK)` — must stay separate groups or they share one identity, which
is the D2 wrong-match class.

**A file directly in the library root has no show folder.** `show_folder_relpath`
returns `""` for it; `db/src/paths.rs:252` asserts exactly that. An empty string
is not a folder that happens to be shared — it is the absence of one. So the
protection inverts: every show in the root lands in one bucket, one group, one
winner, and the group binds all of them to whichever entity won.

Everything derived from the bucket is pooled with it — `library_year`, the
episode and season counts, `library_seasons`, and the reference episode.

## Population, counted

From `scored-a.json` at `6221c59`, selected by the structural property (relpath
with no directory component), not by shape name:

| | rows | measured | correct | wrong.entity | absent | stalled |
|---|---:|---:|---:|---:|---:|---:|
| root-level episode rows | 5,644 | 4,148 | 150 (3.6%) | **2,553** | 1,439 | 1,496 |

The largest measurable failing population in the suite, and the largest
wrong-bind block. Wrong beats absent in severity, so this ranks first on both
counts.

## The convention it depends on, and what it does without it

**The fix depends on the basename carrying the show title.** `tv.root` renders
`Show.S01E01.1080p.WEB-DL.mkv`, so `clean_show_title` recovers `Show` and the
title can carry the grouping.

**Without that convention it does nothing.** A shared root holding
`S01E01.mkv` — no title in the name and no folder to borrow one from — still
groups into a single empty-title bucket. That is not a case this change
improves, and `fix.rs:119` already names it: *"a file sitting directly in the
library root has no folder to borrow from, so the stored title can still be
empty."* This iteration does not address it, and no shape in the oracle
generates it.

So the honest scope is: **a shared root where the filenames carry titles.**

## Dogfood regression risk

The new key is byte-identical to the old one whenever `show_folder` is
non-empty. Only root-level episode files take the new branch. The risk is
therefore confined to libraries that keep episode files directly in the root —
and if any exist, splitting one merged group into per-title groups is the
behaviour the browse path already shows for those files.

Note the two paths currently **disagree**: `visible_show_unit_key` falls back to
the soft key `tv|{query_key(title)}` when the folder has no series row, so browse
already splits root-level files by title while grouping merges them. This change
makes them agree.

Not a wins argument — the library has none of this shape. Purely: what could
break.

## Prediction, written before running

| shape | baseline | predicted | why |
|---|---:|---:|---|
| **tv.root** | 3.6% | **80–90%** | ceiling is `tv.flat` (88.4%), whose basename form is identical and which also gets no folder year |
| tv.root wrong.entity | 2,553 | **< 100** | the merge is the only thing binding 20 shows to one entity |
| every other shape | — | **unchanged** | the key is byte-identical when the folder is non-empty |
| stalls, tv.root | 1,496 | **rises** | 20 real groups ask 20 real queries; the cache holds what one drain fetched, so some will miss |

The stall prediction matters: splitting groups **converts some wrong binds into
stalls, not into correct ones**, because the instrument is collision-poor and
unwarmed. A rise in stalls alongside a fall in wrong is the expected shape, and
`correct%` is over `measured`, so the rate can move for two reasons at once.

If `tv.root` lands near `tv.flat` the mechanism is confirmed. If it lands far
below with stalls absorbing the difference, the fix works and the instrument
cannot see how well — which is a finding about the cache, not about the change.

---

## The change

`queue.rs` grew `episode_group_key`. Episode grouping keys on the show folder as
before, and on `\0{query_key(title)}` when there is no show folder. `\0` cannot
appear in a relpath, so a synthesised key can never collide with a real folder's.

Two keys now, deliberately: `group_key` decides what shares a group, and
`folder_key` reads the folder's stored series identity and stays on the real
relpath, because that is what the `series` table holds. Conflating them would
have made a root-level group look up a series row under a synthesised name.

47 lines, one file, one crate. `nightjar-core` and `nightjar-scanner` are
byte-identical to `origin/main`.

## Measured — four instruments

### 1. The oracle, per shape

Baseline measured on a tree **without** the change, before the change existed.
Both arms `requests=0`. Binaries sha256'd distinct.

| shape | correct b | correct a | wrong b | wrong a | rate b | rate a |
|---|---:|---:|---:|---:|---:|---:|
| **tv.root** | 150 | **2,261** | **2,553** | **8** | 3.6% | **47.8%** |
| every other shape | — | — | — | — | unchanged | unchanged |

**One shape moved. Sixteen are byte-identical.** That is the measured form of
"the key is unchanged when the folder is non-empty" — asserted in the prediction,
confirmed here.

| verdict | before | after | delta |
|---|---:|---:|---:|
| correct | 35,421 | 37,532 | **+2,111** |
| wrong.entity | 2,573 | 28 | **−2,545** |
| partial | 6 | 0 | −6 |
| absent | 8,923 | 9,945 | +1,022 |
| stalled | 21,059 | 20,477 | −582 |
| measured | 46,923 | 47,505 | +582 |
| **correct%** | **75.5%** | **79.0%** | **+3.52 pt** |

Noise floor **0 of 67,982 (0.0000%)**, down from 10. The baseline's 10 churning
rows were all `tv.root` / Star Trek: Discovery — the merged group was the thing
that was unstable, and 6 of them settle as `partial → correct` here.

### 2. The parser sweep

    ./run.sh origin/main loop/matcher-oracle
    74,624 names.  HEAD right BASE wrong 0.  BASE right HEAD wrong 0.  no regression

**Zero because it cannot see this change, not because it saw nothing.** The sweep
builds only `nightjar-core`, which is byte-identical to `origin/main`
(`git diff --quiet origin/main -- server/crates/core` is silent). Recorded as
insensitivity, which is a different claim from a clean result. The brief's known
figure — 48,858 gains, 40 regressions — was the slice against the *old* main;
#149 has since merged the slice, so `origin/main → HEAD` is legitimately 0/0.
No fourth cause, because there are no causes at all.

### 3. The parser corpus

**71.0%** — 844 cases, 738 applicable, 524 pass, 214 fail. Identical to the
brief's number. Parse-level, and it links `nightjar-metadata` only for
`clean_show_title`, so it is also insensitive by construction.

### 4. The dogfood strict pair

    control   sha256 df06d521…   DONE groups=3220 ready=24953 unmatched=51 errors=0 requests=0
    treatment sha256 87578486…   DONE groups=3220 ready=24953 unmatched=51 errors=0 requests=0
    cache 8185 before, 8185 after

`errors=0` and `requests=0` on both arms, so the verdict holds. Distinct
binaries from separate target directories. `NIGHTJAR_REPARSE=1` on both. Every
counter identical — the library has no root-level episode file, so the new branch
never executes there. That is the regression check passing, and it is not a win.

### Shipped tests

740 pass, 0 fail, 3 ignored (pre-existing) across the workspace. Nothing weakened
or deleted.

## Where the prediction missed

**Stalls fell by 582; I predicted they would rise.** The reasoning was that 20
real groups ask 20 real queries and an unwarmed cache would miss some. What
happens instead is the reverse: the merged group asked *one* pooled query and
then walked candidates to shape a 20-show pool, and those candidate calls are
what missed. An honest group asks a simpler question the cache can answer more
often.

The honest reading is that my model of where stalls come from was wrong. They
came from candidate shaping, not from query volume.

The prediction that landed: `tv.root` did not reach `tv.flat`'s 88.4%. It reached
47.8%, and 2,461 rows are `absent`. So the shape of the outcome is the one
predicted — **splitting the group converts wrong binds into absents as much as
into correct binds** — and the reason is the collision-poor cache, which warming
would move and this run could not.

## What it cost — named, not netted

**9 rows went `correct → absent`.** One entity, Dark Matter, 9 episodes, and the
resolver says exactly why:

    unmatched dark matter reason=BelowThreshold {
        confidence: 0.72, method: "exact_title_collision_unpinned" }

Two shows share the title and the filename carries no year, so nothing pins it.
Before the change the pooled 20-show group carried enough spurious evidence —
pooled episode count, pooled seasons — to push it over threshold, and it landed
on the right entity. **That binding was not earned; it was a merged group's
accident.** The code now declines instead of guessing, and an absent binding is
recoverable where a wrong one is not.

It is the M5 mechanism in TV form: an exact title collision with no year.

**8 rows are wrong that were not wrong before** — and they were not correct
before either; they were `stalled`. All 8 are Queer as Folk → Queer as Folk
(2022), the same revival collision that already produces 16 wrong rows in
`tv.flat`, `tv.scene` and the movie shapes. The non-shared-root wrong table is
byte-identical across the pair: 16 Queer as Folk, 4 Blade Runner 2049, both
arms. **No new wrong-bind cause.** The instrument gained visibility into an old
one.

## Verdict — KEEP

Oracle up on the shape targeted and on the total; the other three instruments
clean or provably insensitive; shipped tests green; noise floor down to zero.
The two adverse movements are named above and neither is a new cause.
