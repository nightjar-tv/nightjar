# Nightjar — LLM session entrypoint

Grok also loads [AGENTS.md](AGENTS.md) and `.grok/rules/*` in this repo. Those
spell the plan → implement → review pipeline and meta-doc paths. This file
remains the shared entrypoint for all LLM tools.

Before generating any code or prose in this repository:

1. Read [ENGINEERING_RULES.md](ENGINEERING_RULES.md), the constitution. Refuse
   anything that violates it, citing the rule number.
2. Obey [.cursor/rules/nightjar.mdc](.cursor/rules/nightjar.mdc), how to write
   code that matches those rules in practice.
3. Read [docs/GIT_RULES.md](docs/GIT_RULES.md) for branching, commits, PRs, and
   what never enters the repo. Apply it for every commit message and PR.
4. Establish the current phase and build order before proposing or starting a
   slice: read the constitution and git rules, then read the private meta
   continuity/plan docs (sibling checkout) rather than inferring phase only
   from the code. Local sibling paths (when present):
   - `../nightjar-meta/docs/CONTINUITY.md`
   - `../nightjar-meta/docs/V1_PLAN.md` and/or `../nightjar-meta/docs/PHASE_3_REVISED.md`
   - Before slice close-out: `../nightjar-meta/docs/SLICE_CLOSEOUT.md`
   If meta is missing, ask which phase and build order apply; do not invent them.
5. Match the register of existing docs: plain prose, no marketing voice. See
   [CONTRIBUTING.md](CONTRIBUTING.md) and meta `docs/WRITING_RULES.md` when
   available.
6. Match existing patterns in the crate or module you touch.
7. **Agent models (default):** Grok plans and reviews; DeepSeek implements one
   step at a time against a written plan with acceptance criteria. Implementers
   do not redesign or expand scope. Multi-step automation:
   `/workflow plan-implement-review plan_path="…"`.

Design tokens used by the UI live in `web/src/app.css`. Architecture decisions
live in [docs/adr/](docs/adr/). Working docs (plan, brand, copy deck, writing
checklist) live in the private `nightjar-tv/nightjar-meta` repo (local sibling
`../nightjar-meta` when checked out next to this tree).
