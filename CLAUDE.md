# Nightjar — LLM session entrypoint

Before generating any code or prose in this repository:

1. Read [ENGINEERING_RULES.md](ENGINEERING_RULES.md), the constitution. Refuse
   anything that violates it, citing the rule number.
2. Obey [.cursor/rules/nightjar.mdc](.cursor/rules/nightjar.mdc), how to write
   code that matches those rules in practice.
3. Read [docs/GIT_RULES.md](docs/GIT_RULES.md) for branching, commits, PRs, and
   what never enters the repo. Apply it for every commit message and PR.
4. Establish the current phase and build order before proposing or starting a
   slice: read the constitution and git rules, then ask which phase the
   project is in and what the current build order is, rather than inferring
   either from the code.
5. Match the register of existing docs: plain prose, no marketing voice. See
   [CONTRIBUTING.md](CONTRIBUTING.md).
6. Match existing patterns in the crate or module you touch.

Design tokens used by the UI live in `web/src/app.css`. Architecture decisions
live in [docs/adr/](docs/adr/). Working docs (plan, brand, copy deck, writing
checklist) live in the private `nightjar-tv/nightjar-meta` repo.
