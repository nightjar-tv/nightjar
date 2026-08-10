//! Credential handling for accounts and login sessions (ADR-0034).
//!
//! Its own crate rather than part of `nightjar-core` for a measured reason:
//! `argon2` pulls fourteen nodes, and `core` is a two-dependency leaf that
//! `scanner` and `transcode` both depend on. Neither has any business near
//! credentials, and a crate boundary is what keeps them out of the build graph.

mod hash;
mod token;

pub use hash::{
    ARGON2_MEMORY_KIB, ARGON2_OUTPUT_LEN, ARGON2_PARALLELISM, ARGON2_TIME_COST, HashError,
    VerifyOutcome, hash_password, verify_login, verify_password,
};
pub use token::{
    MintedToken, PROFILE_REF_BYTES, SESSION_TOKEN_BYTES, mint_profile_ref, mint_session_token,
    token_sha256_hex,
};
