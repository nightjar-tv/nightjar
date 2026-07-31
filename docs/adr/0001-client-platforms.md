# ADR-0001: Client platform strategy

- Status: superseded by [ADR-0021](0021-client-architecture.md)
- Date: 2026-07-25
- Superseded: 2026-07-31

## Context

Nightjar needs clients for phone, tablet, and 10-foot TV. The constitution
locked Flutter over a shared Rust player core (Rule 2.4), with Tizen/webOS via
the web UI.

## Decision

Superseded. See ADR-0021 for Flutter UI on every working toolchain,
per-platform engines behind one Dart player interface, the platform table,
and the unresolved Apple engine choice.

## Consequences

Recorded in ADR-0021. Phase 4 client work follows that ADR, not this stub.
