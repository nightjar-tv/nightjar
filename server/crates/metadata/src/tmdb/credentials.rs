//! TMDB credential resolution (ADR-0031 §4).
//!
//! Precedence: `{NIGHTJAR_DATA_DIR}/secrets` field `tmdb_api_key`, else
//! `NIGHTJAR_TMDB_API_KEY`, else the embedded application-key slot (empty
//! until CI injection lands).
//!
//! Secrets file encoding is ADR-0026 §5: line-oriented `name=value`, first
//! `=` separates (values may contain `=`), last assignment wins.

use std::path::{Path, PathBuf};

/// Where the active v3 api_key was taken from (for named refuse on 401/403).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbKeySource {
    SecretsFile,
    Env,
    Embedded,
}

impl TmdbKeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecretsFile => "secrets file",
            Self::Env => "NIGHTJAR_TMDB_API_KEY",
            Self::Embedded => "embedded application key",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbCredentials {
    pub api_key: String,
    pub source: TmdbKeySource,
}

impl TmdbCredentials {
    /// Operator-facing reason when TMDB returns 401/403 for this key.
    /// Override sources must not fall back to embedded (ADR-0031 §4).
    pub fn rejected_reason(&self) -> String {
        match self.source {
            TmdbKeySource::SecretsFile | TmdbKeySource::Env => format!(
                "TMDB API key rejected ({}); not falling back to embedded key",
                self.source.as_str()
            ),
            TmdbKeySource::Embedded => "TMDB embedded application key rejected or revoked".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredError {
    /// No secrets override, no env key, embedded slot empty.
    NoKeyConfigured,
    /// Secrets file exists but could not be read.
    SecretsUnreadable { path: PathBuf, detail: String },
    /// Bearer / JWT presented where only v3 api_key is allowed.
    BearerNotSupported { source: &'static str },
}

impl CredError {
    pub fn operator_reason(&self) -> String {
        match self {
            Self::NoKeyConfigured => "no TMDB API key configured (set tmdb_api_key in \
                 {NIGHTJAR_DATA_DIR}/secrets, or NIGHTJAR_TMDB_API_KEY, \
                 or build with NIGHTJAR_TMDB_APP_KEY)"
                .into(),
            Self::SecretsUnreadable { path, detail } => {
                format!(
                    "TMDB secrets file unreadable ({}): {detail}",
                    path.display()
                )
            }
            Self::BearerNotSupported { source } => format!(
                "TMDB bearer tokens are not supported in v1 ({source}); \
                 use a v3 api_key"
            ),
        }
    }
}

impl std::fmt::Display for CredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.operator_reason())
    }
}

impl std::error::Error for CredError {}

/// Embedded application-key slot (ADR-0031 §3 / §4).
///
/// Readable by the resolver; empty until release CI injects
/// `NIGHTJAR_TMDB_APP_KEY`. This slice wires nothing into the slot.
pub fn embedded_application_key() -> Option<&'static str> {
    option_env!("NIGHTJAR_TMDB_APP_KEY").filter(|s| !s.is_empty())
}

/// Resolve credentials from the process environment and optional data dir.
pub fn resolve_credentials() -> Result<TmdbCredentials, CredError> {
    let data_dir = std::env::var_os("NIGHTJAR_DATA_DIR").map(PathBuf::from);
    let env_key = std::env::var("NIGHTJAR_TMDB_API_KEY").ok();
    resolve_credentials_with(
        data_dir.as_deref(),
        env_key.as_deref(),
        embedded_application_key(),
    )
}

/// Testable resolver: inject data dir, env value, and embedded slot.
pub fn resolve_credentials_with(
    data_dir: Option<&Path>,
    env_api_key: Option<&str>,
    embedded: Option<&str>,
) -> Result<TmdbCredentials, CredError> {
    if let Some(dir) = data_dir {
        let path = dir.join("secrets");
        if let Some(key) = read_secrets_tmdb_api_key(&path)? {
            reject_bearer(&key, "secrets file")?;
            return Ok(TmdbCredentials {
                api_key: key,
                source: TmdbKeySource::SecretsFile,
            });
        }
    }

    if let Some(raw) = env_api_key.map(str::trim).filter(|s| !s.is_empty()) {
        reject_bearer(raw, "NIGHTJAR_TMDB_API_KEY")?;
        return Ok(TmdbCredentials {
            api_key: raw.to_string(),
            source: TmdbKeySource::Env,
        });
    }

    if let Some(raw) = embedded.map(str::trim).filter(|s| !s.is_empty()) {
        reject_bearer(raw, "embedded application key")?;
        return Ok(TmdbCredentials {
            api_key: raw.to_string(),
            source: TmdbKeySource::Embedded,
        });
    }

    Err(CredError::NoKeyConfigured)
}

fn reject_bearer(value: &str, source: &'static str) -> Result<(), CredError> {
    let t = value.trim();
    if t.starts_with("eyJ") || t.to_ascii_lowercase().starts_with("bearer ") {
        return Err(CredError::BearerNotSupported { source });
    }
    Ok(())
}

/// Read `tmdb_api_key` from the secrets file.
///
/// - Missing file → `Ok(None)` (no override).
/// - Present but unreadable → `Err(SecretsUnreadable)`.
/// - Present, no field / empty field → `Ok(None)`.
fn read_secrets_tmdb_api_key(path: &Path) -> Result<Option<String>, CredError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(parse_secrets_field(&raw, "tmdb_api_key")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CredError::SecretsUnreadable {
            path: path.to_path_buf(),
            detail: e.to_string(),
        }),
    }
}

fn parse_secrets_field(raw: &str, field: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == field {
            let v = v.trim();
            found = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn write_secrets(dir: &Path, body: &str) {
        fs::write(dir.join("secrets"), body).unwrap();
    }

    struct PrecedenceCase {
        secrets_body: &'static str,
        env: Option<&'static str>,
        emb: Option<&'static str>,
        expect_src: TmdbKeySource,
        expect_key: &'static str,
    }

    #[test]
    fn precedence_secrets_beats_env_beats_embedded() {
        let cases = [
            PrecedenceCase {
                secrets_body: "tmdb_api_key=from-secrets\n",
                env: Some("from-env"),
                emb: Some("from-embedded"),
                expect_src: TmdbKeySource::SecretsFile,
                expect_key: "from-secrets",
            },
            PrecedenceCase {
                secrets_body: "",
                env: Some("from-env"),
                emb: Some("from-embedded"),
                expect_src: TmdbKeySource::Env,
                expect_key: "from-env",
            },
            PrecedenceCase {
                secrets_body: "# comment only\n",
                env: None,
                emb: Some("from-embedded"),
                expect_src: TmdbKeySource::Embedded,
                expect_key: "from-embedded",
            },
            PrecedenceCase {
                secrets_body: "tmdb_api_key=\n",
                env: Some("from-env"),
                emb: Some("emb"),
                expect_src: TmdbKeySource::Env,
                expect_key: "from-env",
            },
        ];

        for c in cases {
            let dir = tempfile::tempdir().unwrap();
            if !c.secrets_body.is_empty() {
                write_secrets(dir.path(), c.secrets_body);
            }
            let got = resolve_credentials_with(Some(dir.path()), c.env, c.emb).unwrap();
            assert_eq!(
                got.source, c.expect_src,
                "body={:?} env={:?}",
                c.secrets_body, c.env
            );
            assert_eq!(got.api_key, c.expect_key);
        }
    }

    #[test]
    fn no_key_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_credentials_with(Some(dir.path()), None, None).unwrap_err();
        assert_eq!(err, CredError::NoKeyConfigured);
        assert!(err.operator_reason().contains("no TMDB API key"));
    }

    #[test]
    fn missing_data_dir_falls_through_to_env() {
        let got = resolve_credentials_with(None, Some("env-only"), Some("emb")).unwrap();
        assert_eq!(got.source, TmdbKeySource::Env);
        assert_eq!(got.api_key, "env-only");
    }

    #[test]
    fn unreadable_secrets_file_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets");
        fs::write(&path, "tmdb_api_key=secret\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).unwrap();

        let err =
            resolve_credentials_with(Some(dir.path()), Some("from-env"), Some("emb")).unwrap_err();
        // Restore so tempdir cleanup works.
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).unwrap();

        match err {
            CredError::SecretsUnreadable { .. } => {}
            other => panic!("expected SecretsUnreadable, got {other:?}"),
        }
        assert!(
            err.operator_reason().contains("unreadable"),
            "{}",
            err.operator_reason()
        );
        // Must not have fallen through to env.
        assert!(!err.operator_reason().contains("from-env"));
    }

    #[test]
    fn bearer_in_env_rejected() {
        let err = resolve_credentials_with(None, Some("eyJhbGciOi.jwt"), None).unwrap_err();
        assert!(matches!(
            err,
            CredError::BearerNotSupported {
                source: "NIGHTJAR_TMDB_API_KEY"
            }
        ));
    }

    #[test]
    fn bearer_prefix_in_secrets_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_secrets(dir.path(), "tmdb_api_key=Bearer abc.def\n");
        let err = resolve_credentials_with(Some(dir.path()), Some("env"), None).unwrap_err();
        assert!(matches!(
            err,
            CredError::BearerNotSupported {
                source: "secrets file"
            }
        ));
    }

    #[test]
    fn rejected_reason_override_names_no_fallback() {
        let c = TmdbCredentials {
            api_key: "x".into(),
            source: TmdbKeySource::Env,
        };
        let r = c.rejected_reason();
        assert!(r.contains("not falling back to embedded"), "{r}");
        assert!(r.contains("NIGHTJAR_TMDB_API_KEY"), "{r}");
    }

    #[test]
    fn rejected_reason_embedded() {
        let c = TmdbCredentials {
            api_key: "x".into(),
            source: TmdbKeySource::Embedded,
        };
        let r = c.rejected_reason();
        assert!(r.contains("embedded application key rejected"), "{r}");
        assert!(!r.contains("not falling back"), "{r}");
    }

    #[test]
    fn value_may_contain_equals() {
        let dir = tempfile::tempdir().unwrap();
        write_secrets(dir.path(), "tmdb_api_key=ab=cd=ef\n");
        let got = resolve_credentials_with(Some(dir.path()), None, None).unwrap();
        assert_eq!(got.api_key, "ab=cd=ef");
    }

    #[test]
    fn duplicate_key_last_assignment_wins() {
        let dir = tempfile::tempdir().unwrap();
        write_secrets(dir.path(), "tmdb_api_key=first\ntmdb_api_key=second\n");
        let got = resolve_credentials_with(Some(dir.path()), None, None).unwrap();
        assert_eq!(got.api_key, "second");
    }

    #[test]
    fn duplicate_key_empty_last_clears() {
        let dir = tempfile::tempdir().unwrap();
        write_secrets(dir.path(), "tmdb_api_key=first\ntmdb_api_key=\n");
        let got = resolve_credentials_with(Some(dir.path()), Some("from-env"), None).unwrap();
        assert_eq!(got.source, TmdbKeySource::Env);
        assert_eq!(got.api_key, "from-env");
    }

    #[test]
    fn embedded_slot_empty_in_this_build() {
        // This slice does not inject CI secrets; slot must read empty.
        assert!(embedded_application_key().is_none());
    }
}
