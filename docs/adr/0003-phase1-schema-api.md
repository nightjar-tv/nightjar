# ADR-0003: Phase 1 library schema and API shape

- Status: accepted
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
4. **API prefix `/api/v0`.** Additive within v0; breaking changes require `/v1`
   (Rule 2.3 when frozen). OpenAPI is the source of truth; the web client is
   generated from it.
5. **Direct play only.** `playback-info` reports container/codecs and a stream
   URL; remux/transcode are Phase 2. Unplayable probes surface as structured
   `scan_error` / playback reasons, never crashes.
6. Library kinds are `movies` | `shows`. Item kinds are `movie` | `episode` |
   `unknown` from filename parse; metadata matching is Phase 3.

## Consequences

Migrations are irreversible without a new migration. Clients must not invent
endpoints. Streaming paths are ID-based (never raw filesystem paths in URLs) to
keep the Phase 3 path-traversal audit tractable.
