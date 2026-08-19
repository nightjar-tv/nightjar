# Iteration 5 — the harness was not deriving the title the way production does

Instrument work, authorised explicitly after iteration zero reported the gap and
stopped. **This is a re-baseline, not an improvement.** Nothing here makes the
product better, and every rate in notes 00–04 becomes non-comparable to
everything measured after it.

## The gap

`gen_library.py` writes the ground-truth entity name into the capture's `title`
field, so the oracle must not read it — that would be marking its own homework.
`run_one.sh` therefore sets `NIGHTJAR_REPARSE=1`, and the replay re-derives every
field the scanner interprets. For the title it did this:

    let parsed = reparse.then(|| {
        let base = path_str.rsplit('/').next().unwrap_or(path_str);
        nightjar_core::parse_filename(base)
    });
    … Some(p.title.as_str())

**That is a third thing.** Not the capture's field, and not what production
stores. `parse_filename` returns an *empty* title for a name that carries none —
`S01E01.mkv`, `1x04.mkv` — and the scanner then borrows the show folder's name,
at both of its indexing call sites. The harness skipped that step, so:

1. the stored title was empty;
2. an empty title is not a query — `MetadataSource::resolve` filters it to `Miss`
   before any request;
3. `errors=0`, `requests=0`, 693 groups, every one `reason=NoMatch`;
4. **5,644 `tv.numbered` rows scored `absent` at 0.0% correct** for a reason that
   was the harness, not the product.

M1 is already fixed in the product and the instrument could not show it.

## The fix — one rule, three consumers

The scanner's rule was a private `title_from_folder` wrapped in an expression
duplicated at both call sites. Copying that expression into the harness is
exactly what has misreported this project before, so instead:

**Step A, product (`nightjar-scanner`, committed):** extract `stored_title`,
public, and route both indexing paths through it. Rule 4.11 — one filter, three
consumers. A provable pure extraction: the diff removes only the two identical
inline expressions and adds nothing else executable.

**Step B, harness (never committed to the product):** the replay calls
`nightjar_scanner::stored_title`. The patch lives at
`~/nightjar-wt-matcher-scratch/harness-title.patch`, alongside the original
`harness.patch`. `api` already depended on `nightjar-scanner`, and the capture
already carried `library_path`.

### Step A cannot be measured by anything I have

Worth stating plainly. The replay loads a capture straight into `media_items`; it
never runs the scanner. **So both the oracle and the dogfood strict pair are blind
to a scanner change** — as are the sweep (builds `nightjar-core` only) and the
corpus. Step A's verification is the tests plus the diff being a pure extraction,
and that is weaker evidence than a measurement. It is recorded as such.

One test added, `stored_title_substitutes_the_folder_only_for_an_empty_parse`,
asserting the composed rule rather than its halves — the halves each looked
right, and the harness bug lived in the composition.

## Prediction

**Not a blind prediction.** I ran one `tv.numbered` batch through the patched
harness first, so this is informed by that probe and should be read as such:

    before:  DONE groups=693  ready=0     unmatched=5604 errors=0   requests=0
    after:   DONE groups=1151 ready=3594  unmatched=0    errors=235 requests=0

and the stored titles are now `Dept. Q (2025)` — the show folder — as production
would store them.

| shape | now | predicted | why |
|---|---:|---:|---|
| **tv.numbered** | 0.0% | **~100.0%, with ~2,000 stalls** | its folder is `Name (Year)/Season NN/`, identical to `tv.sonarr.plain`'s. Once the title comes from the folder the query is the same, so it should converge on that shape's 3,594 correct / 2,050 stalled |
| **tv.handmade** | all stalled | **still all stalled** | `01 - Closure.mkv` parses to a *non-empty* title, `01 Closure`, so `stored_title` never fires. The query misses the cache and the group stalls. **M2 stays unmeasurable** |
| every other shape | — | **unchanged** | their basenames carry titles, so `stored_title` returns the parsed title untouched |
| overall correct% | 84.3% | **rises ~7 pt** | **an instrument artefact, not a product gain** |

The sharpest test is `tv.numbered` converging on `tv.sonarr.plain` at 3,594. If it
lands somewhere else, the two shapes differ in a way I have not accounted for.

The honest reading of the overall number: it moves because 5,644 rows stop being
scored against a harness defect. **No binding got better.**

---

## Measured

### The re-baseline itself

Same tree as note 04, corrected harness. One shape moved, 16 byte-identical, and
exactly two transitions:

    absent -> correct   3594
    absent -> stalled   2050

| | before | after |
|---|---:|---:|
| tv.numbered | 0.0% | **100.0%** |
| overall correct% | 84.3% | 96.2% |
| overall stalled | 30.9% | **34.0%** |

**`tv.numbered` converged on `tv.sonarr.plain` exactly** — 3,594 measured, 3,594
correct, 2,050 stalled, on both. Checked apart rather than assumed identical:
`S01E01.mkv` against `Dept. Q - S01E01.mkv`, same folder, and zero disagreements
across all 5,644 `(entity, season, episode)` slots. That is the predicted result:
once the title comes from the folder, the two shapes ask the same question.

`tv.handmade` is still 5,644 rows fully stalled. **M2 remains unmeasurable** —
`01 - Closure.mkv` parses to a non-empty title, so `stored_title` never fires, and
the query `01 Closure` is not in the cache. Closing that needs warming, not this.

**No binding got better.** 84.3% → 96.2% is 5,644 rows stopping being scored
against a harness defect, and the denominator shrank: 96.2% is over 44,896
measured rows where 84.3% was over 46,946. Overall stalls **rose** to 34.0%,
because `tv.numbered` now issues 2,050 real queries an unwarmed cache cannot
answer. The rate went up and the coverage went down.

### M1 is already fixed in the product — now shown

The corrected harness run of **`origin/main`** puts `tv.numbered` at
**100.0%, 3,594 correct, 0 absent**. That is iteration zero's claim, measured:
the scanner's folder substitution works, and only the instrument was hiding it.

### The product delta, measured with a correct instrument

`origin/main` + the `stored_title` extraction (control) against the full branch
(treatment), both arms on the corrected harness. **This supersedes every
before/after in notes 00–04.**

| shape | origin/main | after the loop | wrong.entity |
|---|---:|---:|---|
| **tv.root** | 3.1% | **88.4%** | **2,553 → 8** |
| **tv.flat** | 88.4% | **100.0%** | 8 → **0** |
| **tv.flat.titled** | 96.8% | **100.0%** | 0 → 0 |
| the other 14 | — | unchanged | — |

| verdict | origin/main | after | delta |
|---|---:|---:|---:|
| correct | 38,992 | 43,173 | **+4,181** |
| wrong.entity | 2,573 | 20 | **−2,553** |
| partial | 29 | 0 | −29 |
| absent | 3,279 | 1,703 | −1,576 |
| stalled | 23,109 | 23,086 | −23 |
| **correct%** | **86.9%** | **96.2%** | **+9.27 pt** |

**Zero rows that were `correct` on `origin/main` stop being correct.** Checked
directly, not inferred from a transition table I might have truncated again.

That corrects note 01. The 9 Dark Matter rows I reported as `correct → absent`
score **`partial`** on `origin/main` under the corrected harness — a wrong season
mapping inside the merged group, not a binding the code got right. The old
harness's `tv.root` scoring was unstable in exactly that region: all 10 rows of
the original noise floor were `tv.root`, `correct → partial`, on one entity. So
the cost I reported was real against the instrument I had and is not real against
this one. The loop's honest cost is **`partial → absent` 9**, against
`partial → correct` 20.

### The other three instruments

- **Sweep** 74,624 names, 0 gains, 0 regressions. Still insensitive:
  `nightjar-core` remains byte-identical to `origin/main`. **`nightjar-scanner`
  no longer is**, so the earlier "core + scanner byte-identical" claim does not
  carry past note 04 — but the sweep builds neither scanner nor metadata.
- **Corpus** 71.0% (524/738), unchanged.
- **Dogfood strict pair**, corrected harness on **both** arms so the only
  difference is the product: `groups=3220 ready=24953 unmatched=51 errors=0
  requests=0` on both, cache 8185 before and after, distinct binaries. Identical —
  and identical to the pair before the harness fix, which is itself confirmation
  that the dogfood library cannot exercise M1: every basename there carries a
  title, so `stored_title` never substitutes.
- **Tests** 742 in the workspace; 1 pre-existing flaky failure in `transcode`
  (`hls::…_end_moov_mp4_copy_keeps_aac`, a seek-landing assertion that fails
  identically on `origin/main` with no changes). 3 ignored.

## Where the harness patch lives

`~/nightjar-wt-matcher-scratch/harness-title.patch`, beside the original
`harness.patch`. **Not committed to the product**, per the harness commit's own
instruction. Only the `stored_title` extraction is product code.
