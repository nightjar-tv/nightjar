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
