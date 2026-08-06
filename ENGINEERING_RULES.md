# Nightjar — Engineering Constitution
**Status: LOCKED. Changes require unanimous team approval and a commit message explaining what prompted the change.**
This document governs all humans and LLMs contributing to this project. When in doubt, the answer is NO.

---

## 1. The Stack (immutable)

| Layer | Choice | Forbidden alternatives |
|---|---|---|
| Backend | Rust (Axum + Tokio) | No Go, no Node, no Python services |
| Database | SQLite (+ Litestream backup) | No Postgres, no Redis, no ORM |
| Media pipeline | FFmpeg via direct process orchestration | No wrapper libraries |
| Web UI | SvelteKit (PWA-installable) | No React, no second frontend framework |
| Client UI | Flutter where a working toolchain exists; SvelteKit for web | No parallel Swift/Kotlin phone/tablet shells. tvOS SwiftUI only if flutter-tvos is unhealthy (ADR-0021) |
| Playback engines | Per-platform engine behind one Dart player interface (ADR-0021): Media3 on Android; media_kit/libmpv on Windows and Linux; AetherEngine on Apple; vendor players on Tizen/webOS; browser and hls.js on web | No plugin player zoo. No Nightjar-owned decode engine on Tizen/webOS |
| Distribution | Single static binary; Docker as primary channel | No installers, no bundled runtimes |

**Rule 1.1** — Adding any new language, framework, database, or service dependency requires an ADR and unanimous approval. Default answer: no.
**Rule 1.2** — The server ships as ONE binary. Any change that breaks single-binary deployment is rejected.

## 2. Architecture Rules

**Rule 2.1 — Dumb clients, smart server.** All logic (transcode decisions, watch state, metadata, auth, sorting) lives server-side. Clients render API responses and play streams. A client that computes anything the server could compute is a bug.
**Rule 2.2 — The API is the product.** Every feature is API-first. The web UI consumes the same public API as every other client. No private/internal endpoints.
**Rule 2.3 — API stability.** Once v1 is published, endpoints are never broken, only versioned. Additive changes only within a version.
**Rule 2.4 — One player interface.** Playback behaviour (attach, seek, track selection, state, errors) is owned once: the Dart player interface plus the server session contract. Platform engines implement that interface. Do not invent a second OSD, scrubber, or playback-method decision per platform. This replaced "one player core" under Rule 6.4 when the engine bake-off measured that a single Rust/libmpv core could not clear the household platforms, and that per-platform engines are what maximise direct play (ADR-0021).
**Rule 2.5 — FFmpeg is orchestrated, never forked or patched.** We adapt to FFmpeg, not the reverse.

## 3. Scope Rules (v1 lock)

**IN scope:** library scan, metadata (TMDB/TVDB), direct play, remux, hardware transcode, HLS, multi-user auth, watch state/resume, text subtitles (embedded and sidecar, served as WebVTT), image-subtitle burn-in, web UI, Android/Android TV, iOS, tvOS subject to the timeboxed spike and go/no-go in ADR-0001, Tizen/webOS web wrappers.

**OUT of scope for v1 — auto-reject any PR, issue, or suggestion containing:** Live TV, DVR, plugins/extension system, music/photo libraries, federation/multi-server, cloud sync, social features, recommendations/ML, themes beyond light/dark, restyling image subtitles.

**Rule 3.1** — Scope additions require a shipped v1 first. There are no exceptions for "small" features.
**Rule 3.2** — Every feature request gets one of three labels: `v1`, `post-v1`, `never`. Nothing stays unlabeled.

## 4. Debt Prevention Rules

**Rule 4.1 — No TODO merges.** Code with TODO/FIXME/HACK comments does not merge unless linked to a filed issue with an owner.
**Rule 4.2 — CI is law.** Failing CI blocks merge. No force-merges, no "fix it later." CI includes: build, tests, clippy (deny warnings), rustfmt, and the weird-files playback suite.
**Rule 4.3 — The weird-files suite.** A permanent corpus of hostile media (10-bit HEVC, ASS subs, odd audio layouts, broken containers) runs on every PR touching the pipeline. New playback bug = new file in the corpus, forever. Every corpus file must be legally redistributable: FFmpeg-generated or explicitly licensed — never copyrighted commercial media.
**Rule 4.4 — Dependencies are liabilities.** Each new crate/package requires justification in the PR description: what it does, why we can't write it in <200 lines, its maintenance status.
**Rule 4.5 — Delete before you add.** PRs that add a feature should identify what complexity they remove or why net complexity is justified.
**Rule 4.6 — Dogfooding is mandatory.** Main branch runs as the team's real household media server. If you won't run it at home, don't merge it.
**Rule 4.7 — No speculative abstraction.** No traits, generics, or config options for hypothetical future needs. Abstract on the second concrete use case, not the first.
**Rule 4.8 — Incomplete, never provisional.** A slice may do less than the final product, but it may not be built on a design we expect to replace. If the honest description of a slice is "this works for now and we will redo it," stop and design the real thing first. Fewer features is fine; a placeholder architecture is not. Do not hide one behind a "provisional", "temporary", or "good enough for now" label that quietly becomes permanent.
**Rule 4.9 — Data shapes before writers.** Any on-disk or on-wire shape that is expensive to change (segment duration, keyframe cadence, cache keys, schema columns, URL paths that clients bookmark) is decided in an ADR before the code that writes it. Illustration: a frame-count `-g 48` looked like a 2-second GOP until a 60 fps source made segments 0.8s; locking a time-based interval in ADR-0008 is the kind of decision this rule requires up front. The same value was also expressed twice in different units, and the playlist's `TARGETDURATION` disagreed with the encoder's actual segment length until `SEGMENT_MS` became the single owner. Duplicated expressions of one data shape are the same class of bug as choosing the shape badly.
**Rule 4.11 — One concept, one path.** Two things that mean the same to a user are one type with one code path, distinguished by a field, not two implementations that coexist because they arrived in different weeks. An embedded subtitle and a sidecar subtitle are both a subtitle. A remux and a transcode are both a playback session. When a new case arrives, the question is which field it adds, not which branch it needs. If unifying is genuinely wrong, the ADR says why rather than leaving the fork unexplained.
**Rule 4.12 — Works by default.** Core behaviour is correct with no configuration. A setting exists only for a genuine user preference or a hardware escape hatch, and never to avoid making a decision. If the honest description of a knob is "we were not sure, so we made it configurable," decide instead; a value the server can measure is measured, not asked for. The API is the extension point, not plugins and not a config surface. Illustration: `ENCODE_LEAD_SEGMENTS` was a knob standing in for an unmeasured belief about restart latency, and deleting it after ADR-0020 did not move p50. The metadata queue's `max_in_flight` is worse, because the drain is serial per resolve group, so the knob never engaged and tuning it tuned nothing. Both existed because a measurement had not been taken.

## 5. LLM-Specific Rules

**Rule 5.1** — LLMs must read this document before generating any code and must refuse tasks that violate it, citing the rule number.
**Rule 5.2** — LLMs never introduce new dependencies, endpoints, config options, or files outside the established structure without being explicitly asked.
**Rule 5.3** — LLM-generated code follows existing patterns in the codebase. When existing code and this document conflict, this document wins — flag the conflict, don't silently pick.
**Rule 5.4** — No placeholder code. Everything an LLM generates must compile, pass clippy, and include tests for non-trivial logic.
**Rule 5.5** — LLMs asked to add out-of-scope features (Section 3) must respond with the label (`post-v1` or `never`) and stop.

## 6. Process Rules

**Rule 6.1 — ADRs for irreversible decisions.** Any decision that's expensive to undo (schema, API shape, protocol choice) gets a one-page Architecture Decision Record before code.
**Rule 6.2 — One owner per subsystem.** Transcoding, metadata, API, player interface, clients — each has exactly one accountable owner.
**Rule 6.3 — Quarterly rule read-through.** Once per quarter, read this document as a whole for overlap and contradiction, including whether Rules 4.5, 4.7, and 4.11 still say distinct things. Rules may be amended at any time with unanimous agreement when the reasoning is recorded in the commit message.
**Rule 6.4 — The escape hatch.** If a rule is genuinely blocking shipping, write the ADR explaining why, get unanimous sign-off, and amend the rule — don't violate it silently.
**Rule 6.5 — Git.** Branching, commits, PRs, and history hygiene are defined in [docs/GIT_RULES.md](docs/GIT_RULES.md).

---
*If a choice makes the binary bigger, the client smarter, the API weaker, or the scope wider — the answer is no.*
