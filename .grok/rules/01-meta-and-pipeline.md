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
2. **DeepSeek implements** one step at a time: no redesign, no new deps unless the plan and human explicitly require them, match existing patterns. Working tree only; implementer report form (meta VERIFY_TEMPLATES).
3. **Independent verify** (different agent, prefer Grok): structured checklist + acceptance + constitution; bugs fail the step. Verify does not edit code.
4. DeepSeek fix **listed issues only** (max 2 rounds); Grok re-verifies; then escalate if still failing.

Forms and hard gates: `../nightjar-meta/docs/AGENT_PIPELINE.md`,
`../nightjar-meta/docs/VERIFY_TEMPLATES.md`.

Use `/workflow plan-implement-review plan_path="…"` for automation, or the same loop manually with `/model`.

DeepSeek peak pricing (when live): 2× UTC 01:00–04:00 and 06:00–10:00 — prefer off-peak for long multi-step runs.
