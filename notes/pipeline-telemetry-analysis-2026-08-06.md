# Agent pipeline performance and cost report

Analysis of the Nightjar recovery pipeline (RC0-RC8) as recorded in the opencode
message store and server log, 2026-08-04 21:31 through 2026-08-05 16:18. The
current analysis session is excluded. Data caveats: per-message durations
include tool-execution time (opencode keeps a message open while its tools run),
so pure model latency is derived from step/reasoning part timestamps; tool
`state.time` spans for bash are unreliable (recorded sub-100 ms even for
`cargo test --workspace`), so inter-step gaps are used instead.

## 1. End-to-end flow of a typical task (a code slice)

A slice is strictly serial through four tiers, plus human steps:

```
governance (build agent) writes slice into plan  ->  dispatch orchestrator (task tool)
  -> orchestrator reads plan, dispatches implementer (task tool)
     -> implementer: ~100 LLM turns over 16-25 min (read, edit, test, iterate)
     -> orchestrator reads implementer report, dispatches verifier (task tool)
        -> verifier: ~25-34 turns over 6-8 min (read diff, re-run checks)
        -> verifier FAIL -> fix implementer (7-10 min) + re-verify (6 min), max 2 rounds
  -> orchestrator returns dispatch report -> governance decides
  -> human: git commit/PR -> CONTINUITY subagent -> next slice
```

Measured per-slice (RC3-RC8 recovery, 2026-08-05 00:15-05:45, 5 h 30 m wall):

| Slice | Implement | Verify | Fix rounds | Slice total | Notes |
|---|---|---|---|---|---|
| RC3 | 17.6 m | 5.3 m | 0 | ~23 m + 2 h 15 m approval gap | 3 failed dispatches first (agent-not-loaded, then credits) |
| RC4 | 25.5 m | 8.1 m | 0 | 33.6 m | clean run |
| RC5 | 16.1 m | 7.3 m | fix 6.9 + re-verify 7.0 | 37.3 m | 1 fix round |
| RC6 (ADR) | - | 6.7 + 0.9 | 0 | 7.6 m | governance-owned |
| RC7 (ADR) | - | 2.8 m | 0 | 2.8 m | governance-owned |
| RC8 | 24.1 m | 6.0 + 6.0 + 3.4 | fix 10.5 + 6.0 (2 rounds) | 57.0 m | 2 fix rounds, 3 verify runs |

Fix + verify overhead roughly doubles slice time: RC8's 24-minute
implementation cost 57 minutes total.

## 2-4. Every LLM call, by agent, provider, model

2,352 LLM calls across the recovery pipeline (excluding the analysis session):

| Agent | Provider / Model | Calls | Est. cost | Total input (fresh+cache) | Output tok | Reasoning tok |
|---|---|---|---|---|---|---|
| governance (build) | openrouter / claude-opus-5 | 102 | $18.14 | 17.1 M | 90 k | 12 k |
| governance (build) | anthropic / claude-sonnet-5 | 131 | $7.55 | 13.3 M | 50 k | 0 |
| governance (build) | deepseek / deepseek-v4-flash | 318 | $0.42 | 84.1 M | 94 k | 119 k |
| governance (build) | openrouter / qwen3-30b | 33 | $0.02 | 4.5 M | 5 k | 0 |
| implementer | deepseek / deepseek-v4-flash | 575 | $0.53 | 77.9 M | 154 k | 480 k |
| verifier | deepseek / deepseek-v4-flash | 486 | $0.37 | 27.3 M | 128 k | 446 k |
| orchestrator | deepseek / deepseek-v4-flash | 38 | $0.02 | 0.7 M | 7 k | 8 k |
| general | deepseek / deepseek-v4-flash | 16 | $0.01 | 0.6 M | 5 k | 7 k |
| explore | openrouter / claude-opus-5 | 3 | $0.06 | - | - | - |
| Total | | 2,352 | ~$27.1 | ~225 M | 533 k | 1.07 M |

Money is 96% governance. The two governance sessions cost $26.1; the entire
DeepSeek tier (orchestrator + implementer + verifier, 1,099 calls) cost $0.93.
The actual models used were claude-sonnet-5 (not opus-5) for the Block-1
recovery governance, and deepseek-v4-flash for the "dispatcher era" - the
documented grok-4.5 / claude-opus-5 governance tier was not what ran.

## 5-10. Time spent per stage

Pure model-stream time (from step/reasoning part timestamps) and inter-step gaps:

| Agent | Calls | Model stream | Of which reasoning | Tool/turn gaps |
|---|---|---|---|---|
| governance (build) | 788 | 12.8 h | 0.7 h | 10.0 h |
| orchestrator | 38 | 2.5 h* | 0.02 h | 0.01 h |
| implementer | 575 | 1.9 h | 1.1 h | 0.2 h |
| verifier | 486 | 1.7 h | 1.1 h | 0.1 h |

*Orchestrator stream includes subagent execution embedded in its dispatch
steps; its own calls are p50 4 s.

Per-call stream latency (p50 / p90 / p95 / max):
- deepseek-implementer: 3 s / 30 s / 55 s / 250 s
- deepseek-verifier: 3 s / 31 s / 45 s / 408 s
- sonnet-5 governance: 2 s / 47 s / 322 s / 8,100 s
- opus-5 governance: 6 s / 32 s / 60 s / 135 s

Per-session decomposition (RC8-implement as the example): wall 24.1 m = 21.5 m
model stream (of which 10.8 m is "thinking" tokens) + 2.6 m tool work. Subagent
sessions are ~90% model-stream bound; local processing is negligible.

- Subagent launches (task tool): 42 dispatches = 6.5 h of tool time
  (implementer 2.1 h, verifier 1.8 h, orchestrator 2.5 h incl. 3 failed RC3
  dispatches).
- File reads: 591 reads, p50 7 ms / p95 18 ms - fast. ~2.4 h aggregate is one
  2.25 h span artifact. Reads are not a bottleneck.
- Writes: 234 edits + 33 writes + 215 patch parts, ~3 min total.
- Validation (tests): 122 cargo test + 55 other cargo + 13 clippy runs;
  inter-turn gaps total only ~12 min across all implementer work, so tests were
  warm and fast. Not a bottleneck.
- Review/verify: 18 verifier dispatches = 1.8 h, plus governance's own review
  reads (the 2.25 h outlier is governance reading RC3-implement.md). Verify +
  fix loops are the biggest controllable time sink.

## 11-12. Repeated prompts and context reload - confirmed, but cached

- Identical/duplicated prompt content: dispatch prompts are not path-pointered
  as documented. Implementer prompts avg 651 chars (max 1,326), verifier
  prompts avg 1,710 chars (max 4,103) - governance inlines slice text that the
  implementer then re-reads from the plan file. E.g. the RC0 prompt embeds the
  full slice body.
- Repository context reloaded constantly: queue.rs was read 124 times across
  sessions; the rule set (ENGINEERING_RULES, GIT_RULES, nightjar.mdc,
  WRITING_RULES, AGENT_PIPELINE, CONTINUITY) is re-read by nearly every fresh
  session in addition to being in the system prompt; the recovery plan file was
  read 33 times. Every subagent starts with a fresh context (fork_context=false)
  and re-reads the same files from scratch.
- Context grows without bound: the governance session's context grew 2 to
  ~150 k tokens over the recovery with only 2 compaction events.
  Implementer/verifier sessions ship ~135 k tokens of context per call.

## 13. Prompt caching - available and heavily used

Cache hit rate is 98.5-100% for every model (DeepSeek 98.5%, sonnet-5 100%,
opus-5 97.9%). Caching is the only reason 225 M tokens cost $27; without it
this would be several times more expensive. The flip side: every call still
ships ~100-150 k cached tokens through the provider, which is part of the
per-call latency. cache_write reports 0 for DeepSeek, so cache-miss churn is
not visible, but misses are rare at 98.5% hit.

## 14. Parallelism - none

Zero parallelism anywhere. All 42 subagent dispatches are awaited sequentially
(structurally required: verify needs implement output). Slices are executed one
at a time with governance decision gaps between them. Nothing runs concurrently.

## 15. Review/governance loops - the largest controllable overhead

- Fix-verify loops: RC5 = 1 round, RC8 = 2 rounds, RC6 = 1 round, doc pass =
  3 rounds. Each round is a full fresh session (new context, re-reads
  everything).
- The doc-consistency pass needed 3 verifier runs (9.1 + 3.6 + 1.4 min) - the
  "verify ratifies a wrong target" failure mode already documented in
  CONTINUITY.
- RC3 dispatch friction: 3 failed orchestrator attempts (agent not loaded after
  config write; OpenRouter credits exhausted) before the slice ran, plus a
  2 h 15 m governance-approval pause between implement and verify.

## 16-18. Dominant stages

- Latency: model streaming. Governance interactive session (the one the human
  watches): 12.8 h of model stream across the session; per-call p95
  47 s to 5.4 min. Subagent sessions are 90% model-bound. Local processing,
  tests, and file I/O are immaterial.
- Token usage: the governance-on-deepseek era (84 M) and implementer (78 M)
  dominate input volume; reasoning tokens are the hidden tax - implementer
  480 k, verifier 446 k, governance-on-deepseek 119 k.
- Cost: the two governance sessions: $18.14 opus-5 (the one-off drift
  investigation) + $7.55 sonnet-5 (the recovery) = 96% of spend. The DeepSeek
  tier is ~3.4% of cost.

## Prioritised issues

### P1 - Governance spends ~13 h streaming on a slow, long-context model

- Evidence: governance (build) = 12.8 h model stream across 788 calls;
  sonnet-5 p95 322 s; context at 150 k tokens with 2 compactions; the two
  governance sessions are $26.1 of $27.1.
- Impact: this is what "the pipeline feels slow" - the human-facing session
  blocks on 40 s to 5 min calls throughout.
- Fix: compact aggressively (currently near-zero compaction), or split
  governance into shorter, more decisive turns; route routine governance turns
  to the fast tier and reserve sonnet/opus for genuine judgment. Do not hand
  100+ sequential calls to a model at 150 k context.
- Effort: low (config) to medium (workflow change).
- Expected improvement: governance wall time roughly halves; most of the 8.8 h
  of gap plus 12.8 h of stream is compressible.

### P2 - Reasoning tokens double every DeepSeek call's latency and add ~1 M tokens

- Evidence: implementer/verifier each spend ~1.1 h of their ~1.9 h / ~1.7 h
  stream on reasoning parts; 480 k / 446 k reasoning tokens at ~900/call;
  per-call p90 is 30-31 s.
- Impact: the tier that does 1,099 calls pays ~50% extra latency and token
  volume for thinking.
- Fix: evaluate a non-reasoning or lower-reasoning variant for the
  dispatcher/implementer/verifier (the pipeline is prompt-driven, not
  judgement-driven). Verify against the two actual flash failures before
  defaulting.
- Effort: low (model selection).
- Expected improvement: ~40-50% of subagent wall time (roughly 1-1.5 h of the
  RC3-RC8 window) and ~1 M tokens.

### P3 - Verify + fix loops double slice cost and wall time

- Evidence: RC8: 24.1 m implement -> 57 m total (2 fix rounds, 3 verify runs);
  RC5 +1 round; RC6 +1; doc pass 3 rounds. Verify sessions re-read the whole
  context fresh each time.
- Impact: 5 h 30 m of recovery was ~2.7 h of real implement + verify work; the
  rest is re-verification, approval pauses, and dispatcher friction.
- Fix: have the verifier return the specific open_issues list (it does) and the
  fixer address exactly those (it does) - but cut the re-verify cost by reusing
  the original verifier's context/notes instead of a fresh session; treat
  re-verify as incremental, not full re-audit. Apply the documented max-2-round
  gate more aggressively (RC8 hit 2).
- Effort: medium (workflow).
- Expected improvement: 20-40% of the recovery window.

### P4 - Serial slices with governance decision gaps between them

- Evidence: RC3 to RC8 executed one after another with 8-12 min gaps (and one
  2 h 15 m approval pause); 42 of 42 dispatches sequential; zero concurrent
  work.
- Impact: the whole recovery took 5.5 h of continuous effort plus pauses.
- Fix: batch the independent slices (RC6/RC7 ADR verifies are independent of
  RC8 code) so verification of one slice overlaps the next slice's
  implementation; allow the human approval to be a pre-approved batch instead
  of a mid-slice stop.
- Effort: medium.
- Expected improvement: up to 30-50% wall-time reduction on multi-slice
  batches.

### P5 - Every subagent re-loads the same repository context from scratch

- Evidence: queue.rs read 124 times; rule files re-read by every session on top
  of the system prompt; dispatch prompts inline slice text (651-4,103 chars)
  that the implementer re-reads from the plan file; fresh context per subagent
  by design.
- Impact: duplicated tokens and repeated file I/O across every slice;
  contributes to the 225 M token total.
- Fix: (a) obey the documented path-pointering - dispatch prompt = slice id +
  plan path only, so the plan is read once; (b) centralise the always-on rule
  files so they exist in the system prompt only and are not re-read as files;
  (c) share a bounded "recovery brief" context across slices instead of
  cold-starting each.
- Effort: low (prompt discipline) to medium (context sharing).
- Expected improvement: tens of millions of tokens and meaningful per-slice
  start-up time.

### P6 - RC3-style startup friction and approval stalls

- Evidence: 3 failed dispatches (config not reloaded after agent write;
  OpenRouter credits $0), then a 2 h 15 m approval pause; question tool =
  7 asks, 20 min of blocked time.
- Impact: 2.5 h of the recovery window was process, not work.
- Fix: reload agents after config writes (restart opencode before dispatching);
  pre-fund/prefer the DeepSeek route so credits cannot strand a slice; batch
  approval decisions to avoid mid-slice stalls.
- Effort: low.
- Expected improvement: removes the 2 h 15 m stall and ~15 min of dead
  dispatches.

### P7 - The one-off drift session is the single biggest cost item

- Evidence: "Grok drift analysis and Phase 3 recovery": 2 h 4 m, 102 opus-5
  calls, $18.14.
- Impact: 67% of total spend, on an investigation that is now recorded (autopsy
  + plan docs exist).
- Fix: none needed for the pipeline - but the pattern (governance reading six
  large files into context, per AGENT_PIPELINE's own post-mortem) is what made
  it expensive; re-dispatch-on-empty research instead of hand-reading.
- Effort: n/a (process).
- Expected improvement: not repeatable for this spend.

### Not issues (measured)

- File reads/writes and tests are not bottlenecks (reads p95 18 ms; inter-turn
  gaps ~1 s; cargo runs fast on warm target).
- Provider latency is not the problem at the median (p50 3 s) - the problem is
  the number of serial calls and the reasoning tokens.
- Caching is working; do not disable or bypass it.

## Takeaways

The pipeline is not slow because of tooling or I/O - it is slow because it is a
long serial chain of ~2,350 model calls, half of which are spent "thinking", and
because the two governance sessions account for 96% of cost on a slow,
long-context model. The highest-leverage fixes are P2 (drop reasoning for the
mechanical tier), P3 (cheaper re-verify), and P5 (stop re-sending the same
repository context).
