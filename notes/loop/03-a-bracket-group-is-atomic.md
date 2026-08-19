# 03 — a bracket group is atomic

**Kept.** Corpus 390 -> 402 of 738 all-fields (52.8% -> 54.5%). Structure-only
576, unchanged. Zero corpus cases regressed. **Zero dogfood items change at
all** — not one of 25,043.

## Mechanism

A bracket group holding a junk token is release metadata, so the title ends
before the group. When a cut lands inside one it leaves half a bracket:
`Anon Show - Anon Crown [BD 1080p FLAC]` cut at `1080p` gives
`Anon Show - Anon Crown [BD`. `back_up_to_open_bracket` moves the cut out to
the opening bracket. Only a group still **open** at the cut counts, so a
bracket the title closes stays in it.

## The prediction missed, and the miss is the finding

Predicted **+18**. First measurement: **+1**.

The classifier had the count right and the cause wrong. Of the 18 titles
ending in an open bracket, exactly one came from `cut_at_title_junk`, which is
where the rule was written. The other 17 come from cuts the change never
touched:

- the **episode-token cut** — `Anon Show [1x05] An Episode` cuts at the token
  inside the bracket and leaves `Anon Show [`;
- the **year cut** — `[Anon][Anon Title][2019][234]` cuts at `2019` and leaves
  `…[Anon Title][`.

*A fix that removes a route is not a fix for the mechanism.* Rather than revert
for flatness, the rule moved to a single `cut_stem_at` that every stem cut goes
through. Second prediction, traced case by case instead of counted: +9 or +10.
Actual **+12**.

The four `[GM-Team]`-style CJK cases changed and still fail. They need the
leading-bracket mechanism as well, which is its own iteration.

## An instrument defect found here, and what it cost

`analyse.py` compared the two result files through a dict keyed on the input
string. The corpus holds duplicate inputs — `Series.Stagione.3.HDTV.XviD-NOTAG`
appears twice — so the dict compared one case against a different one and
reported both a phantom gain and a phantom loss. The first bracket run showed
`newly failing 1` for a case that passes in both files.

Fixed to compare by position; both files come from the same `cases.json` in the
same order, and the comparison now asserts that the lengths match. Iterations
01 and 02 were **re-verified with the corrected comparison**: `newly failing 0`
in both, unchanged.

## Dogfood

Parse diff over all 25,043 basenames: **0 items change**. Group diff using the
shipped grouping predicates: 0 group keys move, 0 search inputs move, 0 items
bound today at risk. No dogfood title in the library ends in an open bracket,
which is what a library written by a renamer looks like.

## One unrelated red test, named

`nightjar-transcode`'s `hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`
fails with `video starts at 63.646s, land was 58.975s`. It passed during the
iteration-02 gate and fails now on the same tree. Stashing this iteration's
only source change and re-running it reproduces the failure, so it is not
caused by the parser work — it is a timing assertion against a real media file.
Not touched, not weakened, recorded here.
