# Nightjar — Agent entrypoint

This file is loaded automatically by Grok when the session cwd is under this repo.
It does **not** replace the constitution; it tells agents where the binding rules live and how to run the plan → implement → review loop.

## Rules are in your system prompt — do not re-read them as files

The constitution, code rules, git rules, writing rules, pipeline spec, and
continuity brief are pinned into your system prompt via `opencode.json`
`instructions`. They are NOT to be re-read as files: reading them burns a tool
round-trip on content you already hold.

- Constitution: `ENGINEERING_RULES.md`. Refuse violations; cite rule numbers.
- Code style: `.cursor/rules/nightjar.mdc`.
- Git: `docs/GIT_RULES.md` — read it in your head before any branch/commit/PR.
- Prose register: meta `WRITING_RULES.md`.
- Pipeline process: meta `AGENT_PIPELINE.md`.
- What we are doing now: meta `CONTINUITY.md`.

What you still READ as files, once each:

1. **The plan** (`../nightjar-meta/.../PLAN.md` or plan path you were given) —
   your slice's section and the plan's Decisions section. Via the path pointer,
   not inlined text. If you were handed inlined slice text instead of a path,
   refuse and escalate: the caller violated the dispatch contract.
2. **ADRs that touch your slice** — `docs/adr/` entries named in your slice's
   "rules touchpoints" (Rules 4.9, 6.1).
3. The source files your slice changes.

If `../nightjar-meta` is missing, say so and do not invent phase status.

## Agent pipeline (cost / quality) — respect this when planning

**Frozen-plan model.** A planning session terminates by writing a PLAN.md and is
then discarded. It does not persist across days, does not accumulate
exploration context, and never makes inline fixes. Escalation returns a plan
amendment that gets frozen into PLAN.md — never an inline chat fix. A plan Grok
writes for DeepSeek must include, per step:

- **id / title**
- **detail** — concrete files/paths and what to change
- **acceptance** — commands or observables (e.g. `cargo test -p …`, clippy clean)
- **out of scope** — what not to touch
- **rules touchpoints** — which ENGINEERING_RULES / ADR numbers apply

Plans must establish phase and build order (see CLAUDE.md §4); do not infer phase only from code.

Default models for this repo (see `~/.config/opencode/agent/`):

| Role | Model | Job |
|------|--------|-----|
| Planning | `claude-opus-5` / `grok-4.5` (high effort) | Freeze PLAN.md, then the session is discarded. Decides escalations |
| Dispatch | `deepseek-v4-flash` (`nightjar-orchestrator`) | Fresh short session per slice. Input: slice id + plan path ONLY (<5k context). Output: dispatch report |
| Implement | `deepseek-v4-flash` (`nightjar-implementer`) | Smallest change that meets the step; no redesign; plan gaps escalate |
| Verify | `deepseek-v4-flash` (`nightjar-verifier`) | Green-first: tree must compile + test + clippy before any review. Then acceptance + constitution audit. Never edits code |

**Rule engine before model review.** Order is implement → `cargo test` +
`cargo clippy` → ONLY IF GREEN → model review → escalate-or-merge. Review calls
never spend on code that does not compile. The verifier's first check is
mechanical (re-run the gates); only a green tree gets the full audit.

**Do not** put product design or constitution judgments on the dispatcher or the
implementer. Prose, ADRs, design decisions and ops slices stay with governance;
the dispatcher refuses them by design.
**Do not** let implementers expand scope, add dependencies, or invent provisional architecture (Rules 4.4, 4.7, 4.8).

Automated multi-step: `/workflow plan-implement-review plan_path="…"`  
That workflow must respect the dispatch contract: slice id + plan path only, no
inlined slice text.

**Independent verify (hard gate):** after implement, a *different* agent checks
the gates (compile/test/clippy green) then acceptance + constitution;
implementer self-report is not enough. Copy-paste forms live in private meta:
`../nightjar-meta/docs/VERIFY_TEMPLATES.md` (with `AGENT_PIPELINE.md`). Max two
fix rounds, then escalate to the human. Do not put long process essays into this
public tree.

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

## Approval is via PR, not a live gate

A finished slice becomes a branch + PR. Approval is `nightjar-meta/scripts/slice_approval.py` (zero LLM calls in the poll loop), or the human reviewing the PR. Approval happens at batch boundaries, not per slice, and never blocks the next independent slice.

## Also load

[`CLAUDE.md`](CLAUDE.md) — original session entrypoint (same hierarchy).
