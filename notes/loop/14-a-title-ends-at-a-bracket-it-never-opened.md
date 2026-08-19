# 14 — a title ends at a bracket it never opened

**Kept.** Corpus 488 -> 494 of 738 all-fields (66.1% -> 66.9%). Structure-only
612, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

## Mechanism

The mirror of iteration 03. That rule backed a cut **out of** a bracket the
title had opened and not closed; this one ends the title **at** a bracket it
closes without ever having opened:

    Anime Series Title][12END][720p]   ->   Anime Series Title

Everything from the stray `]` on belongs to a group the title is not part of.

**Square brackets only.** Parentheses appear inside real titles — an Arabic
corpus case that passes today carries a stray `)`, and cutting there breaks it.
Measured: including parentheses gains nothing extra and costs that case. `[`
and `]` are release-group syntax and nothing else.

## Prediction vs actual

Predicted **+6**, structure unchanged, 0 dogfood items.
Actual **+6**, structure unchanged, **0** dogfood items.

## Where these names come from, stated plainly

They reach the parser already mangled, and the gain is partly an artefact of
the harness rather than of the parser.

The corpus stores release *names*, and several use `/` as a dual-title
separator: `[哥布林杀手/Anime Series Title][12END][720p]`. `corpus_run.rs` calls
`basename`, which splits on `/` because a real path does. So the parser is
handed the tail of the name with its opening `[` already gone.

The rule is right either way — a title cannot contain a bracket it never
opened, and that happens without any `/` too — but these six cases are **not**
evidence that those release names parse correctly as whole names. A harness
that passed the release name intact would ask a different and harder question.

## The ceiling, measured

With this landed, 244 failures remain. Counted from the case file rather than
estimated:

| cause | cases | can the parser earn it? |
|---|---:|---|
| addressable in the parser | 179 | yes |
| wants an empty title | 29 | parser can, caller cannot — see notes/loop/13 |
| season lives in a path component | 22 | no; the runner passes the basename by design |
| corpus stores a squashed title | 9 | no; a harness artefact |
| corpus stores absent as `0` | 5 | no; `None` can never equal `Some(0)` |

**The ceiling for a parse-level, parser-only change is 673/738 = 91.2%.**
Today's 494 is 66.9%.
