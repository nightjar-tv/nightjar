# Contributing

Read [ENGINEERING_RULES.md](ENGINEERING_RULES.md) first. It is short, and it is
the law here: stack, scope, and how AI-assisted contributions are handled.

Git workflow (branches, commits, PRs) is in [docs/GIT_RULES.md](docs/GIT_RULES.md).
Everything reaches `main` via PR; squash-merge only.

Docs are plain prose, no marketing voice. Match the register of existing docs
(especially the ADRs and [docs/LITESTREAM.md](docs/LITESTREAM.md)).

New playback bugs come with a sample file under `testdata/` (Rule 4.3). PRs that
shrink the codebase are the most welcome kind.
