# Nightjar — Grok / agent entrypoint

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

Default models for this repo (see `~/.grok/config.toml`):

| Role | Model | Job |
|------|--------|-----|
| Plan / design / review | `grok-4.5` (high effort) | Architecture, ADRs, slice plans, constitution checks |
| Implement | `deepseek-flash` (or `deepseek-pro` if stuck) | Smallest change that meets the step; no redesign |
| Re-check | `grok-4.5` | Diff vs plan + ENGINEERING_RULES; bugs block pass |

**Do not** put product design or constitution judgments solely on DeepSeek.  
**Do not** let implementers expand scope, add dependencies, or invent provisional architecture (Rules 4.4, 4.7, 4.8).

Automated multi-step: `/workflow plan-implement-review plan_path="…"`  
That workflow must load this file’s mandatory reads on every implement and review step.

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
