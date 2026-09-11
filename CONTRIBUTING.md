# Contributing

Anyone may report a bug or suggest a change. Implementation contributions are
by invitation: ask a maintainer before you write code. We may close an
unsolicited implementation pull request without reviewing the patch.

This policy governs what this project accepts upstream. It does not limit the
rights the GPL-3.0-only licence grants you to use, study, modify, redistribute
or fork the software, or to run it as an independent service.

## Reports and suggestions

Use GitHub Issues for a reproducible bug report or a feature suggestion. We
triage them against the supported scope (Rule 3.2) and do not promise to merge
unsolicited code. Report a security issue through the private route in
[SECURITY.md](SECURITY.md), never a public issue.

## Invited contributions and the DCO

Every invited contribution needs a `Signed-off-by` line. Add it with
`git commit -s`. The sign-off certifies the
[Developer Certificate of Origin 1.1](DCO), the standard text at
<https://developercertificate.org/>: that you created the work or have the right
to submit it under the project licence. It is not a copyright assignment, and it
grants no right to relicense your work under proprietary terms. You keep your
copyright.

The invitation covers the GPL-3.0-only server and embedded web UI. It does not
cover the proprietary official native clients or the operated Plus service.
Contributor rights for those components are not settled, and this GPL and DCO
workflow does not infer them.

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
