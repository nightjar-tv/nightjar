# Nightjar — Agent entrypoint

This file is loaded automatically by Grok when the session cwd is under this repo.
It does **not** replace the constitution; it tells agents where the binding rules live and how to run the plan → implement → review loop.

## Mandatory reads before any code or design output

1. **Constitution** — [`ENGINEERING_RULES.md`](ENGINEERING_RULES.md). Refuse violations; cite rule numbers (Rules 5.1–5.5).
2. **How to write code here** — [`.cursor/rules/nightjar.mdc`](.cursor/rules/nightjar.mdc) (always-on Cursor/Grok rules).
3. **Git** — [`docs/GIT_RULES.md`](docs/GIT_RULES.md) before any branch, commit, or PR.
4. **Prose** — [`CONTRIBUTING.md`](CONTRIBUTING.md); for copy/register also meta writing rules (below).
5. **ADRs that touch the work** — [`docs/adr/`](docs/adr/). Data shapes and API before writers (Rules 4.9, 6.1).
6. **Phase / continuity (sibling private repo)** — when planning or slicing, also read:
   - `../nightjar-meta/docs/CONTINUITY.md`
   - `../nightjar-meta/docs/V1_PLAN.md` and/or `../nightjar-meta/docs/PHASE_3_REVISED.md` as relevant
   - `../nightjar-meta/docs/SLICE_CLOSEOUT.md` before declaring a slice done
   - `../nightjar-meta/docs/WRITING_RULES.md` for public prose

If `../nightjar-meta` is missing, say so and do not invent phase status.

## Agent pipeline (cost / quality) — respect this when planning

Default models for this repo (see `~/.config/opencode/agent/`):

| Role | Model | Job |
|------|--------|-----|
| Governance | `claude-opus-5` / `grok-4.5` (high effort) | Architecture, ADRs, slice plans, constitution checks, decides escalations |
| Dispatch | `deepseek-v4-flash` (`nightjar-orchestrator`) | Runs one **code** slice: implement, verify, max two fix rounds. Decides nothing |
| Implement | `deepseek-v4-flash` (`nightjar-implementer`) | Smallest change that meets the step; no redesign; plan gaps escalate |
| Verify | `deepseek-v4-flash` (`nightjar-verifier`) | Acceptance + constitution audit; bugs block pass; never edits code |

**Do not** put product design or constitution judgments on the dispatcher or the
implementer. Prose, ADRs, design decisions and ops slices stay with governance;
the dispatcher refuses them by design.
**Do not** let implementers expand scope, add dependencies, or invent provisional architecture (Rules 4.4, 4.7, 4.8).

Automated multi-step: `/workflow plan-implement-review plan_path="…"`  
That workflow must load this file’s mandatory reads on every implement and review step.

**Independent verify (hard gate):** after implement, a *different* agent checks
acceptance + constitution; implementer self-report is not enough. Copy-paste
forms live in private meta: `../nightjar-meta/docs/VERIFY_TEMPLATES.md`
(with `AGENT_PIPELINE.md`). Max two fix rounds, then escalate to the human.
Do not put long process essays into this public tree.

## Governance token discipline

You (governance, this session) are the expensive model. Every line you hold in
context or generate costs more than the same line at any other tier. Full
rationale and the incident that forced this: `AGENT_PIPELINE.md` § Governance
token discipline.

- Dispatch the dispatcher for code slices; never a code slice directly to
  `nightjar-implementer` or `nightjar-verifier`.
- When a research subagent (`explore`, `general`) returns empty or thin,
  **re-dispatch it** with a sharper prompt. Do not re-do its job by reading
  source yourself — that is how a governance session's context grows unbounded.
- Read subagent report **pointers**, not bodies. Open a report file only when
  the one-line summary is not enough to decide the next step.
- Quote a source file to the user with a `file:line` reference, not a pasted
  block, unless the user needs to read the exact text to make a decision.
- If your last two replies restated the same plan or the same finding, stop:
  say so and ask what changed, rather than writing a third restatement.

## Planning output contract

A plan Grok writes for DeepSeek must include, per step:

- **id / title**
- **detail** — concrete files/paths and what to change
- **acceptance** — commands or observables (e.g. `cargo test -p …`, clippy clean)
- **out of scope** — what not to touch
- **rules touchpoints** — which ENGINEERING_RULES / ADR numbers apply

Plans must establish phase and build order (see CLAUDE.md §4); do not infer phase only from code.

## Close-out

Before merge claims: run product mechanical checks (CI / `nightjar-meta/slice-check.sh` when available) and answer every question in `../nightjar-meta/docs/SLICE_CLOSEOUT.md`.

## Also load

[`CLAUDE.md`](CLAUDE.md) — original session entrypoint (same hierarchy).
