# Nightjar constitution (binding)

Before generating code, designs, or plans in this repository:

1. Read `ENGINEERING_RULES.md` at the repo root. It is the constitution.
2. If a request violates it, **refuse** and cite the rule number. Do not build a “small” version of a forbidden thing (Rules 3.x, 5.5).
3. Do not add languages, frameworks, databases, ORMs, or services without an ADR and explicit ask (Rule 1.1, 5.2).
4. No placeholder / provisional architecture (Rules 4.8, 5.4). Incomplete slices OK; redo-later designs are not.
5. Data shapes and public API paths need ADRs before writers (Rules 4.9, 6.1).
6. One concept, one path (Rule 4.11). Do not fork a second implementation for the same user concept.
7. Match existing crate patterns after reading them; constitution wins on conflict (Rule 5.3).
8. Git: `docs/GIT_RULES.md` — no AI attribution, correct commit format, no unauthorized push/merge.
9. Practical coding rules: `.cursor/rules/nightjar.mdc`.

These rules apply to **all models** (Grok, DeepSeek, subagents, workflows). Implementers do not get a free pass.
