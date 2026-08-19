# 15 — the episode marker may come after the number

**Kept.** Corpus 494 -> 499 of 738 all-fields (66.9% -> 67.6%). Structure-only
612, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

## Mechanism

Turkish releases write the episode as `69. Blm`, `60.Bolum`, `1. Bölüm` — the
number first, then the word for "episode". Same family as the `Ep01` marker,
with the halves reversed.

## The library is silent here, and that is not the same as safe

**0 of 25,043 dogfood paths contain any of these words.** Unlike iteration 06,
where the library held 1,272 paths containing `episod` and the rule fired on
none of them, this is genuine silence: the library can neither confirm nor
refute. The guard is the grammar — a digit run must come first, and the word
must not be glued to another letter — and both are tests. That is weaker
evidence than the structural rules had and it is stated rather than glossed.

## A byte-length trap, caught before it shipped

The first attempt folded the title with `to_lowercase()` so `BÖLÜM` would
match. **`to_lowercase()` is not length-preserving** — a Unicode fold can
change a character's byte length — and `cut_at_episode_marker` indexes the
folded copy and then slices the *original*. The two would drift apart and the
slice would be wrong or panic.

Reverted to `to_ascii_lowercase`, which preserves length. The cost is that a
non-ASCII letter is not folded, so `BÖLÜM` in capitals is missed while `Bölüm`
matches. Every measured case is the latter, and the comment on the function now
says why the weaker fold is the right one.

## Prediction vs actual

Predicted **+5**, structure unchanged, 0 dogfood items.
Actual **+5**, structure unchanged, **0** dogfood items.
