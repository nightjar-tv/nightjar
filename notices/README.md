# Release notices

This directory holds the committed notice material for Nightjar releases. The
root `NOTICE` names the shipped runtime components and points here.

- `THIRD-PARTY.md` — generated Rust and web runtime dependency notices.
- `rust-licenses.json` — the committed publisher-declared license expression
  map for the exact packages in `server/Cargo.lock`.
- `Apache-2.0.txt` — the Apache License 2.0 text.
- `hls.js-1.6.16.txt` — the notice shipped with hls.js 1.6.16.

## Regeneration

`scripts/check_notices.py generate` rewrites `THIRD-PARTY.md` from
`server/Cargo.lock`, `web/package-lock.json` and `rust-licenses.json`.
`scripts/check_notices.py check` fails when the committed file drifts.
`scripts/check_notices.py selftest` proves the checker can fail.

## Provenance

The Rust expressions are the publishers' declared crates.io metadata, recorded
for the exact locked versions in `server/Cargo.lock`. Workspace entries use the
workspace license field. They are not an audit of file-level copyright notices.
The map records the same declared expressions as the R2 release bill of
materials (nightjar-meta `docs/R2_RELEASE_BOM_2026-09-11.md`, Appendix A).
Update the map when a lock change adds or moves a crate.
