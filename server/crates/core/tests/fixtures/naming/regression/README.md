# regression/

Reserved for cases promoted out of the inline `#[test]` cases in
`server/crates/core/src/filename.rs`.

Ownership: the core crate. A case lands here only after the inline test that
covers it exists and passes. Promotion copies the case into this root; it does
not move or delete the inline test.

No cases are present yet.
