# Contributing

Read [ENGINEERING_RULES.md](ENGINEERING_RULES.md) first. It is short, and it is
the law here: stack, scope, and how AI-assisted contributions are handled.
Client platforms and playback engines are in
[docs/adr/0021-client-architecture.md](docs/adr/0021-client-architecture.md).

Git workflow (branches, commits, PRs) is in [docs/GIT_RULES.md](docs/GIT_RULES.md).
Everything reaches `main` via PR; squash-merge only.

Docs are plain prose, no marketing voice. Match the register of existing docs
(especially the ADRs and [docs/LITESTREAM.md](docs/LITESTREAM.md)).

New playback bugs come with a sample file under `testdata/` (Rule 4.3). PRs that
shrink the codebase are the most welcome kind.

## Playback behaviour changes

Before changing how sessions, playlists, or segment responses behave, capture
the client's actual request sequence (ordered playlist and segment GETs with
status codes) from attach through the action under test — for example audio
switch or scrub. One capture answers questions that rounds of speculative
playlist edits will not. Prefer server-side request logging on the dogfood
binary over guessing from browser console snippets alone.
