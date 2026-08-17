# Nightjar — Git Rules

Referenced by [CLAUDE.md](../CLAUDE.md) and [.cursor/rules](../.cursor/rules).
Commit messages and PR bodies follow the plain-prose register in
[CONTRIBUTING.md](../CONTRIBUTING.md).

## 1. Branching model
- **Trunk-based.** `main` is the only long-lived branch and is always
  releasable. It runs our homes (Rule 4.6).
- Work happens on short-lived branches: `<area>/<slug>` (e.g.
  `scanner/non-utf8-paths`, `web/dusk-strip-focus`, `docs/adr-0003`). Areas
  match top-level directories.
- Branches live days, not weeks. If a branch is older than 5 working days,
  split the work or land it behind a flag.
- No `dev`, no `release/*` branches, no git-flow. Releases are tags on `main`.

## 2. Commits
- **Format:** `area: imperative subject under 72 chars`
  - `scanner: handle symlink loops during walk`
  - `transcode: cap concurrent sessions at config limit`
  - `api: add /v1/items/{id}/playback-info`
  - Areas: `server`, `api`, `web`, `player`, `apps`, `db`, `scanner`,
    `transcode`, `docs`, `ci`, `testdata`. Cross-cutting: pick the dominant one.
- Subject is plain English, imperative, no trailing period, no emoji, no
  `feat:`/`fix:` prefixes, no AI attribution lines or Co-Authored-By bots.
- Body only when the *why* isn't obvious from the diff. Wrap at 72. Explain
  the reasoning or the FFmpeg quirk, not the mechanics.
- Reference issues as `Fixes #123` in the body, never in the subject.
- Each commit compiles and passes tests on its own. No "wip", "fix", "fix2",
  "asdf" commits reach `main` (squash handles branch mess; see §3).
- Migrations, API spec changes, and their implementation land in the SAME
  commit. The repo is never in a half-contract state.

## 3. Pull requests
- Everything reaches `main` via PR, including founders, including one-line
  fixes. No direct pushes (branch protection enforces).
- **Squash-merge only.** Linear history; the PR title becomes the commit
  subject, so PR titles follow §2 format. CI lints the title.
- One PR = one concern. If the description needs "and also", split it.
- PR description answers: what, why, how verified. New dependency? Include the
  Rule 4.4 justification. Playback fix? Name the corpus file added (Rule 4.3).
- Review: at least the subsystem owner (CODEOWNERS routes this). Self-merge is
  allowed only for `docs/` typo-level changes with green CI. While the project
  has a single maintainer, self-merge on green CI is the working rule; required
  review returns as soon as there is a second owner.
- No TODO/FIXME/HACK in the diff (Rule 4.1). CI greps for it.
- Draft PRs are fine and encouraged for early feedback; never merge a draft.
- **`--auto` is not "merge when green" here, and four occurrences say so.**
  Branch protection requires `web` and `server`. `gate1` is **not required**, and
  it is the slowest check by a wide margin — measured at 400 s and 340 s on
  2026-08-11 with the required pair already green. `gh pr merge --auto` fires the
  moment the *required* set passes, so it merges before `gate1` has said anything.
  It has done so twice and been fine by luck. **Wait for `gate1` explicitly**
  (`gh pr checks <pr>` until it reads `pass`), or make it required. Until it is
  required, "CI is green" and "the required checks are green" are different
  claims and only one of them is what `--auto` reads.

## 4. What never enters the repo
- Secrets, API keys, tokens. CI runs a secret scanner; a leaked key means
  rotate it immediately. History rewrite won't save you.
- Media files outside `testdata/` (which is Git LFS only). No sample movies
  "temporarily" anywhere else.
- Generated code is not committed, with one exception: generated API clients
  are committed, with a CI job that regenerates and fails on diff. Rationale:
  the web build needs the client at build time, API changes stay reviewable in
  PRs, and the drift check keeps it honest. Other generated artifacts (token
  CSS, etc.) remain build outputs.
- Editor/OS junk (.DS_Store,.idea). Covered by.gitignore; don't add
  exceptions.
- Vendored dependencies. Cargo/npm lockfiles yes; copied source trees no.
- Working docs that do not bind contributors (plan, brand strategy, copy deck,
  writing checklist). Those live in a private maintainer repository.

## 5. Tags & releases
- Semantic versioning: `v1.2.3`. Tags are annotated and signed, created only
  from green `main`.
- Pre-1.0: `v0.x.y`, breaking changes allowed with a CHANGELOG note. Post-1.0:
  API breaking changes are forbidden within /v1 (Rule 2.3).
- CHANGELOG.md is written for users, updated in the release PR. Not
  auto-generated commit spam.

## 6. History hygiene
- Never force-push `main` or rewrite public history. Force-push to your own
  branch freely before review; after a review starts, push new commits so
  reviewers can see deltas (squash erases them at merge anyway).
- Revert cleanly with `git revert`. A bad merge on `main` gets reverted first,
  investigated second (main must stay releasable).
- Rebase your branch on `main` rather than merging `main` into it.

## 7. LLM-specific
- LLMs write commit messages and PR descriptions to this spec, and never
  include model names, "generated by", or tooling attribution in either.
- An LLM never runs `git push`, tags, or merges **except under the session-scoped
  authorization below**. Humans pull the trigger on anything that leaves the
  machine — authorizing it in-session is one way of pulling it.
- **This section governs this repository.** Its authorization discipline exists
  because a bad push here reaches something other people run. A maintainer's
  private working notes are a different risk and are governed where they live,
  not from here — a rule should not reach into a repository it cannot name.
- **Session-scoped publish carve-out.** A human may authorize an LLM to branch,
  commit, push, and open pull requests for one session. Publish is allowed only
  when the human authorized it that turn.

  > **Amended 2026-08-15.** This clause previously justified itself by saying it
  > reused a mechanism the private maintainer tooling already had, "so both repos
  > share one shape instead of growing a second". That rationale is withdrawn
  > along with the cross-repo claim above it: the shapes do not have to match,
  > and asserting they do is how this section came to be read as governing a
  > repository it cannot name. The carve-out stands on its own terms.

  Rule 4.11 applies to process, not only to code.

  Authorization is never standing. It does not survive the session that granted
  it, and a later session needs its own.

  The carve-out never extends to rebase, force-push, tags, pushing to `main`,
  deleting branches, or touching a PR after it is opened.

  > **Amended 2026-08-17. Merge was on that list and is not any more.**
  >
  > The clause previously read: *"The carve-out never extends to merge, rebase,
  > force-push, tags, pushing to `main`, deleting branches, or touching a PR
  > after it is opened. The human approves and merges at the PR, so nothing
  > reaches `main` without them and the line above keeps its point: the trigger
  > that matters is the merge, not the push. That is also what bounds the damage
  > — a bad push under this carve-out produces a branch nobody merges."*
  >
  > It was then waived twice in three days — 2026-08-15 for eleven stacked pull
  > requests, and 2026-08-17 for #129 — each time recorded as one instance and
  > not a precedent. **A rule waived whenever it binds is worse than no rule**,
  > because it still reads as a constraint to anyone who was not in the room,
  > and the record of what actually happens lives only in the waivers.
  >
  > So the exclusion is withdrawn rather than waived a third time. **Merge is
  > permitted on the same terms as push: named, in-session, and recorded before
  > it runs.** What that changes is the ceremony, not the control — the human
  > still decides, per merge, in that session.
  >
  > **What is deliberately kept:** merge is authorized per pull request, never
  > standing for a session the way push is. "You may push this session" does not
  > authorize a merge; "merge #129" authorizes one merge. And the authorization
  > note still precedes the action, so `git log` carries the ordering.

  A merge under this carve-out is authorized **per pull request**, by name. A
  session-scoped push grant does not carry a merge with it.

  Nothing reaches `main` without a human saying so, which is what the line at
  the top of this section means. **What bounds the damage is that the human
  names the pull request**, not that a machine is barred from running the
  command.

  **Why it exists, at the size the evidence supports.** A large publishing
  session is twenty-odd branch pushes and eighteen pull request bodies, and the
  bodies are the load-bearing part: they carry merge order, add/add conflicts,
  and migration warnings that a reviewer needs and that the diff does not show.
  Eighteen of those written by hand is where fatigue drops the one that
  mattered.

  It is **not** justified by tree safety. An earlier draft of this clause
  claimed that splitting an uncommitted working tree by hand was the danger,
  citing two sessions that damaged a tree doing it. The session that prompted
  the clause then measured the actual split and found most content already
  committed and the residue small, so the danger it named was largely absent.
  Toil is the reason. The corrected sentence is kept here rather than quietly
  replaced, because a rule carrying a false premise is the failure this document
  exists to catch, and it is worse in the rule than anywhere else.

  **The authorization note is committed before the first push — and before a
  merge — not after.** It records the date and the scope in the session
  handoff, and for a merge it names the pull request. Committing it first
  puts the ordering in `git log`, so the constraint is auditable instead of
  trusted — a push with no authorization commit before it is a violation on the
  face of the history, visible to someone who was not in the room. An unrecorded
  push is a violation even when a human authorized it in chat, because the tree
  is what a later reader has.
- If an LLM's change spans areas or needs a migration, it says so in the PR
  description explicitly rather than burying it.

---
*First-commit checklist: this file + scaffold as one commit
(`repo: scaffold monorepo per V1 plan Phase 0`), then branch protection ON
before the second commit exists.*
