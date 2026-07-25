# ADR-0001: Client platform strategy

- Status: proposed
- Date: 2026-07-25

## Context

Nightjar needs clients for phone, tablet, and 10-foot TV. The constitution
locks Flutter over a shared Rust player core (Rule 2.4), with Tizen/webOS via
the web UI.

## Decision

Pending sign-off. Working assumption per ENGINEERING_RULES.md: Flutter for
Android / Android TV / iOS; web UI for Tizen/webOS; tvOS go/no-go after a
timeboxed flutter-tvos spike.

## Consequences

Recorded once signed. Do not start Phase 4 client work until this ADR is
accepted.
