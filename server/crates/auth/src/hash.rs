//! Argon2id password hashing (ADR-0034 item 4).

use argon2::password_hash::{
    Error as PhError, PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use argon2::{Algorithm, Argon2, Params, Version};
use std::sync::LazyLock;

/// RFC 9106's second recommended parameter set, chosen against the slowest
/// machine we claim to run on (ADR-0034 item 4). Not settings: raising them is
/// an ADR amend, and old hashes keep working because the parameters travel
/// inside the PHC string.
pub const ARGON2_MEMORY_KIB: u32 = 19456;
pub const ARGON2_TIME_COST: u32 = 2;
pub const ARGON2_PARALLELISM: u32 = 1;
pub const ARGON2_OUTPUT_LEN: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub enum HashError {
    /// The stored string is not a PHC hash this server can read.
    Malformed,
    /// A PHC hash in some algorithm other than Argon2id. Refused rather than
    /// upgraded (ADR-0034 item 4): silently accepting one would mean the
    /// parameters above were never applied.
    WrongAlgorithm,
    /// Hashing failed, which is a fault rather than a wrong password.
    Backend,
}

/// What a verify attempt concluded. Three states rather than a bool, because
/// "correct but stored under weaker parameters" needs a rehash and "correct"
/// does not, and a caller cannot tell them apart from `true`.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    Correct,
    /// Correct, and the stored hash is below the current constants. Verified
    /// against its own encoded parameters; rehash on this successful login.
    CorrectNeedsRehash,
    Wrong,
}

fn hasher() -> Result<Argon2<'static>, HashError> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_TIME_COST,
        ARGON2_PARALLELISM,
        Some(ARGON2_OUTPUT_LEN),
    )
    .map_err(|_| HashError::Backend)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password into a PHC string. The salt is 16 bytes from the OS CSPRNG,
/// which is `SaltString::generate`'s recommended length.
pub fn hash_password(password: &str) -> Result<String, HashError> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(hasher()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| HashError::Backend)?
        .to_string())
}

/// Verify against a stored PHC string.
///
/// The comparison uses the parameters encoded in `stored`, not the constants
/// above, so a hash written under weaker settings still verifies. It is
/// reported as [`VerifyOutcome::CorrectNeedsRehash`] so the login path can
/// upgrade it.
pub fn verify_password(password: &str, stored: &str) -> Result<VerifyOutcome, HashError> {
    let parsed = PasswordHash::new(stored).map_err(|_| HashError::Malformed)?;
    if parsed.algorithm != Algorithm::Argon2id.ident() {
        return Err(HashError::WrongAlgorithm);
    }
    match hasher()?.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(if is_below_current(&parsed) {
            VerifyOutcome::CorrectNeedsRehash
        } else {
            VerifyOutcome::Correct
        }),
        Err(PhError::Password) => Ok(VerifyOutcome::Wrong),
        // Any other variant is a fault in the stored value rather than a
        // rejected password, and the login route must not read it as one.
        // Note the failure direction: if password-hash ever returns something
        // other than `Password` for a genuine mismatch, that mismatch reads as
        // `Malformed`. It still fails closed, but with the wrong reason.
        Err(_) => Err(HashError::Malformed),
    }
}

/// A real Argon2id hash under the current parameters, of a password nobody
/// holds. Its only job is to give [`verify_login`] something to do when the
/// username does not exist.
///
/// Built once, lazily, because the parameters are memory-hard by design and
/// paying that cost per failed login for a nonexistent user would be a denial
/// of service rather than a mitigation.
static ABSENT_ACCOUNT_HASH: LazyLock<String> = LazyLock::new(|| {
    hash_password("nightjar: no account holds this password")
        .expect("compiled-in Argon2 parameters must be valid")
});

/// Verify a login attempt against the stored hash, or against a dummy when the
/// account does not exist.
///
/// **This is the only entry point a login route should use, and the `Option`
/// is why.** With separate functions, "no such user" is a branch someone can
/// take before the expensive path, and that branch is a user-enumeration
/// oracle: at m=19456 KiB the difference is tens of milliseconds, not
/// microseconds, and trivially measurable over a LAN. Here the work is the
/// same either way because there is no shape of call that skips it.
///
/// ADR-0034 item 10 accepts one enumeration-adjacent exposure in the bootstrap
/// window, deliberately and with reasons. Accepting a second one by omission
/// would be a different thing.
pub fn verify_login(password: &str, stored: Option<&str>) -> Result<VerifyOutcome, HashError> {
    match stored {
        Some(stored) => verify_password(password, stored),
        None => {
            // Result discarded on purpose: the answer is always Wrong. The
            // call is here for the time it takes, not for what it returns.
            let _ = verify_password(password, &ABSENT_ACCOUNT_HASH);
            Ok(VerifyOutcome::Wrong)
        }
    }
}

/// Whether any encoded cost is below the current constant. Only downgrades
/// trigger a rehash: a hash stored under *stronger* settings than today's is
/// left alone, because rewriting it would weaken it.
fn is_below_current(parsed: &PasswordHash<'_>) -> bool {
    let Ok(params) = Params::try_from(parsed) else {
        return true;
    };
    params.m_cost() < ARGON2_MEMORY_KIB
        || params.t_cost() < ARGON2_TIME_COST
        || params.p_cost() < ARGON2_PARALLELISM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_the_wrong_password() {
        let stored = hash_password("correct horse battery staple").unwrap();
        assert_eq!(
            verify_password("correct horse battery staple", &stored).unwrap(),
            VerifyOutcome::Correct
        );
        assert_eq!(
            verify_password("Correct horse battery staple", &stored).unwrap(),
            VerifyOutcome::Wrong
        );
        assert_eq!(verify_password("", &stored).unwrap(), VerifyOutcome::Wrong);
    }

    /// Two hashes of one password differ, which is the salt doing its job.
    #[test]
    fn salt_is_per_hash() {
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b);
        assert_eq!(verify_password("same", &a).unwrap(), VerifyOutcome::Correct);
        assert_eq!(verify_password("same", &b).unwrap(), VerifyOutcome::Correct);
    }

    /// ADR-0034 item 4: the parameters travel with the hash, so they must
    /// actually be written into it. Reading them back off the string is what
    /// proves they were applied rather than defaulted.
    #[test]
    fn phc_string_carries_the_adr_parameters() {
        let stored = hash_password("x").unwrap();
        assert!(stored.starts_with("$argon2id$"), "{stored}");
        let parsed = PasswordHash::new(&stored).unwrap();
        let params = Params::try_from(&parsed).unwrap();
        assert_eq!(params.m_cost(), ARGON2_MEMORY_KIB);
        assert_eq!(params.t_cost(), ARGON2_TIME_COST);
        assert_eq!(params.p_cost(), ARGON2_PARALLELISM);
        assert_eq!(params.output_len(), Some(ARGON2_OUTPUT_LEN));
        let mut salt_buf = [0u8; 64];
        let salt = parsed.salt.unwrap().decode_b64(&mut salt_buf).unwrap();
        assert_eq!(salt.len(), 16, "16-byte salt");
    }

    /// The negative case ADR-0034 item 4 names: a hash that is not argon2id is
    /// refused, not upgraded. Without this the algorithm could be defaulted
    /// away and every other test here would still pass.
    #[test]
    fn a_non_argon2id_hash_is_refused() {
        let params = Params::new(
            ARGON2_MEMORY_KIB,
            ARGON2_TIME_COST,
            ARGON2_PARALLELISM,
            Some(ARGON2_OUTPUT_LEN),
        )
        .unwrap();
        let other = Argon2::new(Algorithm::Argon2i, Version::V0x13, params);
        let salt = SaltString::generate(&mut OsRng);
        let stored = other.hash_password(b"x", &salt).unwrap().to_string();
        assert!(stored.starts_with("$argon2i$"), "{stored}");
        assert_eq!(
            verify_password("x", &stored),
            Err(HashError::WrongAlgorithm),
            "the right password under the wrong algorithm is still refused"
        );
    }

    #[test]
    fn a_weaker_stored_hash_verifies_and_asks_to_be_rehashed() {
        let weak = Params::new(8, 1, 1, Some(ARGON2_OUTPUT_LEN)).unwrap();
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, weak);
        let salt = SaltString::generate(&mut OsRng);
        let stored = argon.hash_password(b"x", &salt).unwrap().to_string();
        assert_eq!(
            verify_password("x", &stored).unwrap(),
            VerifyOutcome::CorrectNeedsRehash
        );
        assert_eq!(verify_password("y", &stored).unwrap(), VerifyOutcome::Wrong);
    }

    #[test]
    fn a_stronger_stored_hash_is_left_alone() {
        let strong = Params::new(
            ARGON2_MEMORY_KIB * 2,
            ARGON2_TIME_COST + 1,
            ARGON2_PARALLELISM,
            Some(ARGON2_OUTPUT_LEN),
        )
        .unwrap();
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, strong);
        let salt = SaltString::generate(&mut OsRng);
        let stored = argon.hash_password(b"x", &salt).unwrap().to_string();
        assert_eq!(
            verify_password("x", &stored).unwrap(),
            VerifyOutcome::Correct,
            "rehashing a stronger hash would weaken it"
        );
    }

    #[test]
    fn an_absent_account_is_wrong_not_an_error() {
        assert_eq!(
            verify_login("anything", None).unwrap(),
            VerifyOutcome::Wrong
        );
    }

    /// The mitigation is that both paths do the same work, so this asserts the
    /// thing that makes that true: the dummy is a real Argon2id hash carrying
    /// the current parameters, not a placeholder that verifies instantly.
    ///
    /// Deliberately not a timing assertion. A wall-clock comparison in a test
    /// is flaky under CI load and would be the kind of green-but-meaningless
    /// check that proves nothing; the cost parameters are the property that
    /// actually determines the work.
    #[test]
    fn the_absent_account_hash_costs_what_a_real_one_costs() {
        let dummy = &*ABSENT_ACCOUNT_HASH;
        assert!(dummy.starts_with("$argon2id$"), "{dummy}");
        let parsed = PasswordHash::new(dummy).unwrap();
        let params = Params::try_from(&parsed).unwrap();
        assert_eq!(params.m_cost(), ARGON2_MEMORY_KIB);
        assert_eq!(params.t_cost(), ARGON2_TIME_COST);
        assert_eq!(params.p_cost(), ARGON2_PARALLELISM);
    }

    /// Even a caller who somehow supplied the dummy's own plaintext gets
    /// `Wrong`, because the absent-account result is discarded rather than
    /// returned. Without this the dummy would be a universal password.
    #[test]
    fn the_dummy_password_does_not_admit_anyone() {
        assert_eq!(
            verify_login("nightjar: no account holds this password", None).unwrap(),
            VerifyOutcome::Wrong
        );
    }

    /// A present account still routes through the real hash, so the Option is
    /// a source selector and not a bypass.
    #[test]
    fn a_present_account_still_verifies_normally() {
        let stored = hash_password("hunter2").unwrap();
        assert_eq!(
            verify_login("hunter2", Some(&stored)).unwrap(),
            VerifyOutcome::Correct
        );
        assert_eq!(
            verify_login("wrong", Some(&stored)).unwrap(),
            VerifyOutcome::Wrong
        );
    }

    #[test]
    fn malformed_storage_is_not_a_wrong_password() {
        for bad in ["", "not a hash", "$argon2id$", "plaintext"] {
            assert_eq!(
                verify_password("x", bad),
                Err(HashError::Malformed),
                "{bad}"
            );
        }
    }
}
