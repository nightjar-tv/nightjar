# Contributing

Read [ENGINEERING_RULES.md](ENGINEERING_RULES.md) first. It is short, and it is
the law here: stack, scope, and how AI-assisted contributions are handled.

## Reading order

After the constitution, five ADRs orient you to how the system is actually
shaped — read these before the rest, in this order. The full register,
every ADR with its status and what supersedes what, is
[docs/adr/README.md](docs/adr/README.md); start there when you need a
specific decision, come here first when you don't know which one yet.

1. [ADR-0021](docs/adr/0021-client-architecture.md) — what the clients are
   (Flutter UI, per-platform playback engines) and why there is more than
   one engine.
2. [ADR-0011](docs/adr/0011-remux-session-convergence.md) — the session
   model every playback path (direct play, remux, transcode) converges
   onto.
3. [ADR-0022](docs/adr/0022-capability-profiles.md) — how server and
   client agree on what a given client can play, which is the contract
   the session model in ADR-0011 decides against.
4. [ADR-0025](docs/adr/0025-item-identity.md) — the identity model
   nearly everything else in the data layer depends on (watch state,
   metadata, track selection).
5. [ADR-0026](docs/adr/0026-metadata-pipeline.md) — the other large
   subsystem: how titles get matched, matter to first-screen latency,
   and where two-tier status comes from.

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
