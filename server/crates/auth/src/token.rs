//! Opaque session tokens and durable profile references (ADR-0034 items 5, 6).

use argon2::password_hash::rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

/// 256 bits, per ADR-0034 item 5.
pub const SESSION_TOKEN_BYTES: usize = 32;
/// 128 bits rendered as 32 lowercase hex characters, per ADR-0034 item 6.
pub const PROFILE_REF_BYTES: usize = 16;

/// A freshly minted login token: the plaintext the client is handed exactly
/// once, and the digest the server stores.
///
/// Returned together so a caller cannot store the plaintext by reaching for
/// the wrong field; the only way to get the storable form is to ask for it.
#[derive(Debug)]
pub struct MintedToken {
    pub plaintext: String,
    pub sha256_hex: String,
}

/// Mint a login token from the OS CSPRNG.
///
/// SHA-256 rather than Argon2 for the stored form is deliberate (ADR-0034
/// item 5): the token is a full-entropy random value rather than a guessable
/// secret, and it is hashed on every request, so a slow KDF would buy nothing
/// and cost per-request latency.
pub fn mint_session_token() -> MintedToken {
    let plaintext = random_hex(SESSION_TOKEN_BYTES);
    let sha256_hex = token_sha256_hex(&plaintext);
    MintedToken {
        plaintext,
        sha256_hex,
    }
}

/// The stored form of a presented token. One function, so the login path and
/// the verify path cannot disagree about what is compared (Rule 4.11).
pub fn token_sha256_hex(plaintext: &str) -> String {
    hex(&Sha256::digest(plaintext.as_bytes()))
}

/// Mint a durable `profile_ref`. Not the rowid: SQLite reuses rowids after a
/// delete, so profile 4 deleted and recreated would inherit the old profile's
/// history (ADR-0034 item 6).
pub fn mint_profile_ref() -> String {
    random_hex(PROFILE_REF_BYTES)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    hex(&buf)
}

/// Lowercase hex, written locally rather than pulling a rendering crate
/// (ADR-0034 item 4, Rule 4.4).
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_token_is_256_bits_of_hex_and_its_digest_is_stored() {
        let minted = mint_session_token();
        assert_eq!(minted.plaintext.len(), SESSION_TOKEN_BYTES * 2);
        assert!(minted.plaintext.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            minted.plaintext.chars().all(|c| !c.is_ascii_uppercase()),
            "lowercase hex, so one rendering reaches storage"
        );
        assert_eq!(minted.sha256_hex.len(), 64);
        assert_eq!(minted.sha256_hex, token_sha256_hex(&minted.plaintext));
        assert_ne!(
            minted.sha256_hex, minted.plaintext,
            "the stored form is not the credential"
        );
    }

    #[test]
    fn profile_refs_are_128_bits_of_lowercase_hex() {
        let r = mint_profile_ref();
        assert_eq!(r.len(), PROFILE_REF_BYTES * 2, "32 hex characters");
        assert!(
            r.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    /// Not a randomness test, which a unit test cannot be. It catches the
    /// failure that would actually happen: a constant, a reused buffer, or a
    /// counter, any of which collides immediately.
    #[test]
    fn minting_does_not_repeat_itself() {
        let tokens: HashSet<String> = (0..64).map(|_| mint_session_token().plaintext).collect();
        assert_eq!(tokens.len(), 64);
        let refs: HashSet<String> = (0..64).map(|_| mint_profile_ref()).collect();
        assert_eq!(refs.len(), 64);
    }

    #[test]
    fn hashing_is_stable_and_input_sensitive() {
        assert_eq!(token_sha256_hex("abc"), token_sha256_hex("abc"));
        assert_ne!(token_sha256_hex("abc"), token_sha256_hex("abd"));
        // Known vector, so a swapped algorithm is caught rather than assumed.
        assert_eq!(
            token_sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
