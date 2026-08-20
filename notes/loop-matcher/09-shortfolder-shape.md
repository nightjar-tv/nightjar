# Iteration 9 — the shape that costs ADR-0047's stronger option

Added before implementing anything, because it is the regression guard for the
admission fix and not only for the option nobody is taking. Implementing "an
exact fold beats an extension" without a shape that exercises the legitimate
extension would have been a change with no way to see its own risk.

## The shape

`tv.shortfolder`: a folder under the head of a colon-named show —
`Quiet on Set: The Dark Side of Kids TV` in a folder called `Quiet on Set` — in
`tv.noyear`'s filename form. **No year, deliberately**: a folder year lets the
year pin resolve it without the title-extension rule ever firing, and the rule is
what is being measured.

Entities untouched at 2,410. Rows 73,625 → 73,738.

## The filter had to be tightened, and that is the finding

The first cut checked the truncated head against other *oracle* entities and
passed 29 shows. **Eleven of those are shows TMDB itself also lists under the bare
head.** A folder called `Monarch` could honestly mean `Monarch`; `Spartacus`,
`Spartacus`. For those the oracle has no answer and asserting one manufactures
bad-oracle rows — the defect that put six wrong answers through the parser sweep.

Ambiguity has to be tested against the provider, not against the kept set. Every
exclusion is now counted and printed rather than left as a silent narrowing:

    two colon names share this head                  29
    provider also lists a show under the bare head   11
    head is another kept entity's full name           4
    head shorter than 3 characters                    1
    head's search not cached — unverifiable           1

17 shows survive, 113 rows. A head whose search is not cached is excluded too: an
answer that cannot be verified is not one to assert.

## Measured — 116 live requests, then `requests=0`

| | rows | correct | absent | wrong.unk |
|---|---:|---:|---:|---:|
| tv.shortfolder | 113 | **58 (51.3%)** | 54 | 1 |

Nine of the ten matched groups bound via **`exact_title`** — the extension
admitted, no surviving competitor, so it took the sole-hit branch at 0.90.

## What it settles, and what it does not

**Settles**: the admission rule is load-bearing, and it is worth **58 rows** here.
ADR-0047 recorded that cost as unmeasured and I had assumed it was near-total. It
is not. Option B — dropping the arm — trades ~58 correct for ~80 wrong, which
under the leave bar is defensible on its own terms.

**It also makes C the dominant option on evidence rather than argument.** In 4 of
5 sampled queries there is no exact-fold competitor at all, and by the filter's
construction there is none in any surviving row — so "an exact fold beats an
extension" keeps all 58 *and* removes the 80. C strictly dominates B.

**Does not settle**: 113 rows against 80 wrong bindings from the same rule. This
shape can show the rule is load-bearing. **It cannot show the rule is worth its
cost** — the populations are not comparable, one being bounded by how many
colon-named shows have an unambiguous head. And a provider name longer *without* a
colon — `The Office` → `The Office US` — is still not generated.
