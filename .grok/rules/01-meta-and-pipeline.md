# Nightjar meta docs + agent pipeline

## Sibling repo: nightjar-meta

Strategy, continuity, phase order, and slice close-out live in the private sibling repo:

`../nightjar-meta/` (relative to the nightjar product root).

When **planning**, **slicing**, or **closing** work, also read with tools:

| Doc | When |
|-----|------|
| `../nightjar-meta/docs/CONTINUITY.md` | New design/strategy session; avoid repeating known mistakes |
| `../nightjar-meta/docs/V1_PLAN.md` / `PHASE_3_REVISED.md` | Phase and build order |
| `../nightjar-meta/docs/SLICE_CLOSEOUT.md` | Before “slice done” / merge readiness |
| `../nightjar-meta/docs/WRITING_RULES.md` | Public docs, ADRs, user-facing copy |

If the path is missing, report it; do not invent continuity or phase status.

## Plan → implement → review (required shape)

1. **Grok plans** (high effort): multi-step plan with acceptance criteria and rule/ADR citations.
2. **DeepSeek implements** one step at a time: no redesign, no new deps unless the plan and human explicitly require them, match existing patterns.
3. **Grok reviews**: acceptance + constitution; bugs fail the step.
4. Optional DeepSeek fix rounds; Grok re-checks.

Use `/workflow plan-implement-review plan_path="…"` for automation, or the same loop manually with `/model`.

DeepSeek peak pricing (when live): 2× UTC 01:00–04:00 and 06:00–10:00 — prefer off-peak for long multi-step runs.
