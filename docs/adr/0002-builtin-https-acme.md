# ADR-0002: Built-in HTTPS / ACME

- Status: proposed
- Date: 2026-07-25

## Context

Self-hosters need TLS for remote access. Options: require a reverse proxy,
or embed ACME (Let's Encrypt) in the Nightjar binary.

## Decision

Pending sign-off. Decide before Phase 3 HTTPS work. Default bias: keep the
binary simple; document Tailscale / reverse-proxy paths first; embed ACME
only if Gate 3 household testing shows proxy friction is a real blocker.

## Consequences

Affects binary size, attack surface, and install docs. Irreversible enough
to require this ADR before code (Rule 6.1).
