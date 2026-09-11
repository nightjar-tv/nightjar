# ADR-0003: Phase 1 library schema and API shape

- Status: accepted (items 3 and 4 superseded 2026-09-07)
- Date: 2026-07-25

## Context

Phase 1 needs durable library/item storage and a public API for scan → list →
direct play. Schema and `/v0` shapes are expensive to undo (Rule 6.1).

## Decision

1. **SQLite** in the Nightjar data directory (`NIGHTJAR_DATA_DIR`, default
   `./data`), WAL mode, numbered append-only migrations in `server/crates/db`.
2. **Integer primary keys** for libraries and media items. Stable path identity
   is `(library_id, path)` with a UNIQUE constraint; path bytes are stored as
   UTF-8 with lossy fallback recorded separately when needed.
3. **No auth in v0.** Single-user local trust. Auth arrives in Phase 3.
4. **API prefix `/api/v0`.** OpenAPI is the source of truth; the web client is
   generated from it. Follow [Rule 2.3](../../ENGINEERING_RULES.md): API
   versioning serves supported official clients. Breaking changes require a
   verified official-client rollout and compatibility plan, not a third-party
   support window. Use a version or capability distinction when needed to keep
   supported official clients working through the rollout. *Superseded
   2026-09-07:* the former additive-only and mandatory-new-major policy no
   longer governs. Original decision: **API prefix `/api/v0`.** Additive within
   v0; breaking changes require `/v1` (Rule 2.3 when frozen). OpenAPI is the
   source of truth; the web client is generated from it.
5. **Direct play only.** *Superseded by ADR-0006 (2026-07-25), which adds
   remux delivery and replaces the `directPlay`/`needsTranscode` fields with
   `playbackMethod`.* Original decision: `playback-info` reports
   container/codecs and a stream URL; remux/transcode are Phase 2. Unplayable
   probes still surface as structured `scan_error` / playback reasons, never
   crashes.
6. Library kinds are `movies` | `shows`. Item kinds are `movie` | `episode` |
   `unknown` from filename parse; metadata matching is Phase 3.

### Historical supersession — authentication (2026-09-07)

[ADR-0034](0034-accounts-and-profiles.md) supersedes decision 3's statement
that v0 has no auth. The original decision and its Phase 1 local-trust rationale
are retained as historical record; this supersession does not change decision
4's API prefix or its versioning policy. Current implementation references are
the OpenAPI bearer/cookie schemes and auth routes,
`server/crates/api/src/routes/auth.rs`, and the accounts/profiles/sessions
migration under `server/crates/db/migrations/`.

## Consequences

Migrations are irreversible without a new migration. Clients must not invent
endpoints. Streaming paths are ID-based (never raw filesystem paths in URLs) to
keep the Phase 3 path-traversal audit tractable.
