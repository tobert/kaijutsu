//! Choosing an SSH key source from a flag pair plus its environment
//! fallbacks — the one resolver `kaijutsu-mcp`, `kaijutsu-acp`,
//! `kaijutsu-app`, and `kaijutsu-tui` all call, so `--key-fingerprint` and
//! `--key-file` mean the same thing in every binary that accepts them.
//!
//! [`resolve_key_source`] is the pure core: it takes plain values, never
//! reads the environment itself, and is fully testable without touching
//! `std::env`. [`KeyArgs`] (behind the `cli` feature) is the thin edge that
//! reads `KAIJUTSU_KEY_FINGERPRINT`/`KAIJUTSU_KEY_FILE` and calls it.

use std::path::PathBuf;

use crate::ssh::KeySource;

/// Resolve `flag_fingerprint`/`flag_file` against their environment
/// fallbacks (`env_fingerprint`/`env_file`, read by the caller from
/// `KAIJUTSU_KEY_FINGERPRINT`/`KAIJUTSU_KEY_FILE`) into a [`KeySource`].
///
/// A flag wins over its variable. An environment variable set to the empty
/// string counts as unset, not as a value naming an empty fingerprint or
/// path — an exported-but-blank `KAIJUTSU_KEY_FINGERPRINT=""` must not
/// collide with a `--key-file` flag. A flag given as the empty string is
/// different: it is a deliberate argument, not ambient environment, so it
/// fails immediately with [`KeySelectError::EmptyFingerprint`] or
/// [`KeySelectError::EmptyFile`] rather than being silently dropped.
///
/// Naming both a fingerprint and a file (after resolving variables) is
/// [`KeySelectError::BothGiven`], which names both values and both variable
/// names — never a silent pick of one over the other. Naming neither keeps
/// `KeySource::Agent`, trying every key the agent holds.
pub fn resolve_key_source(
    flag_fingerprint: Option<String>,
    flag_file: Option<PathBuf>,
    env_fingerprint: Option<String>,
    env_file: Option<String>,
) -> Result<KeySource, KeySelectError> {
    if let Some(fingerprint) = &flag_fingerprint
        && fingerprint.is_empty()
    {
        return Err(KeySelectError::EmptyFingerprint);
    }
    if let Some(file) = &flag_file
        && file.as_os_str().is_empty()
    {
        return Err(KeySelectError::EmptyFile);
    }

    let fingerprint = flag_fingerprint.or_else(|| env_fingerprint.filter(|s| !s.is_empty()));
    let file = flag_file.or_else(|| env_file.filter(|s| !s.is_empty()).map(PathBuf::from));

    match (fingerprint, file) {
        (Some(fingerprint), Some(file)) => Err(KeySelectError::BothGiven {
            fingerprint,
            file: file.display().to_string(),
        }),
        (Some(fingerprint), None) => Ok(KeySource::agent_key(fingerprint)),
        (None, Some(file)) => Ok(KeySource::from_file(file)),
        (None, None) => Ok(KeySource::Agent),
    }
}

/// Errors [`resolve_key_source`] returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeySelectError {
    /// `--key-fingerprint` was given as the empty string.
    #[error("--key-fingerprint must name an SSH SHA256 fingerprint, not the empty string")]
    EmptyFingerprint,
    /// `--key-file` was given as the empty string.
    #[error("--key-file must name a path, not the empty string")]
    EmptyFile,
    /// A fingerprint and a file both resolved to a value, from any
    /// combination of flag and environment variable.
    #[error(
        "--key-fingerprint ({fingerprint}) and --key-file ({file}) (or their \
         KAIJUTSU_KEY_FINGERPRINT/KAIJUTSU_KEY_FILE variables) name two \
         different keys; give exactly one"
    )]
    BothGiven { fingerprint: String, file: String },
}

/// `--key-fingerprint`/`--key-file`, shared verbatim across every binary
/// that connects over SSH. Flatten this into a `clap::Parser` struct with
/// `#[command(flatten)]` rather than declaring the pair again.
#[cfg(feature = "cli")]
#[derive(clap::Args, Debug, Clone)]
pub struct KeyArgs {
    /// SSH agent identity to connect as, given as its OpenSSH
    /// `SHA256:<base64>` fingerprint (the string `ssh-add -l` prints).
    /// Falls back to `KAIJUTSU_KEY_FINGERPRINT`; a flag wins over its
    /// variable. Default, with `--key-file` also unset: try every key the
    /// agent holds. A fingerprint the agent does not offer fails the
    /// connection rather than falling back to another key.
    #[arg(long)]
    pub key_fingerprint: Option<String>,

    /// Private key file to connect with, read directly instead of through
    /// the SSH agent. Falls back to `KAIJUTSU_KEY_FILE`; a flag wins over
    /// its variable. Default, with `--key-fingerprint` also unset: try
    /// every key the agent holds. The file must be unencrypted — an
    /// encrypted key fails the connection instead of prompting for a
    /// passphrase — and giving both `--key-fingerprint` and `--key-file`
    /// (after resolving their variables) is an error.
    #[arg(long)]
    pub key_file: Option<PathBuf>,
}

#[cfg(feature = "cli")]
impl KeyArgs {
    /// Resolve these flags against `KAIJUTSU_KEY_FINGERPRINT`/
    /// `KAIJUTSU_KEY_FILE`. The environment read stays at this thin edge so
    /// [`resolve_key_source`] itself stays pure and testable.
    pub fn key_source(&self) -> Result<KeySource, KeySelectError> {
        resolve_key_source(
            self.key_fingerprint.clone(),
            self.key_file.clone(),
            std::env::var("KAIJUTSU_KEY_FINGERPRINT").ok(),
            std::env::var("KAIJUTSU_KEY_FILE").ok(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_agent_when_nothing_given() {
        let source = resolve_key_source(None, None, None, None).expect("no error");
        assert!(matches!(source, KeySource::Agent));
    }

    #[test]
    fn flag_fingerprint_wins_over_its_env_var() {
        let source = resolve_key_source(
            Some("SHA256:flag".to_string()),
            None,
            Some("SHA256:env".to_string()),
            None,
        )
        .expect("no error");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:flag"),
            other => panic!("expected AgentKey, got {other:?}"),
        }
    }

    #[test]
    fn env_fingerprint_used_when_flag_unset() {
        let source = resolve_key_source(None, None, Some("SHA256:env".to_string()), None)
            .expect("no error");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:env"),
            other => panic!("expected AgentKey, got {other:?}"),
        }
    }

    #[test]
    fn flag_file_wins_over_its_env_var() {
        let source = resolve_key_source(
            None,
            Some(PathBuf::from("/flag/key")),
            None,
            Some("/env/key".to_string()),
        )
        .expect("no error");
        match source {
            KeySource::File { path, passphrase } => {
                assert_eq!(path, PathBuf::from("/flag/key"));
                assert_eq!(passphrase, None);
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn env_file_used_when_flag_unset() {
        let source =
            resolve_key_source(None, None, None, Some("/env/key".to_string())).expect("no error");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/env/key")),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn both_given_after_resolution_is_an_error() {
        // Both from flags.
        let err = resolve_key_source(
            Some("SHA256:flag".to_string()),
            Some(PathBuf::from("/flag/key")),
            None,
            None,
        )
        .expect_err("both a fingerprint and a file must be refused");
        assert!(matches!(err, KeySelectError::BothGiven { .. }));
        let text = err.to_string();
        assert!(text.contains("SHA256:flag"));
        assert!(text.contains("/flag/key"));

        // One from a flag, the other from its variable — still both given.
        let err = resolve_key_source(
            Some("SHA256:flag".to_string()),
            None,
            None,
            Some("/env/key".to_string()),
        )
        .expect_err("a flag plus the OTHER option's env var must still be refused");
        let text = err.to_string();
        assert!(text.contains("SHA256:flag"));
        assert!(text.contains("/env/key"));

        // Both from variables.
        let err = resolve_key_source(
            None,
            None,
            Some("SHA256:env".to_string()),
            Some("/env/key".to_string()),
        )
        .expect_err("two identities from variables alone must not resolve to one");
        let text = err.to_string();
        assert!(text.contains("SHA256:env"));
        assert!(text.contains("/env/key"));
    }

    #[test]
    fn empty_env_fingerprint_does_not_collide_with_a_file_flag() {
        // An exported-but-blank KAIJUTSU_KEY_FINGERPRINT must read as unset,
        // not as a fingerprint naming the empty string — otherwise a
        // --key-file flag would spuriously error as "both given".
        let source = resolve_key_source(
            None,
            Some(PathBuf::from("/flag/key")),
            Some(String::new()),
            None,
        )
        .expect("an empty env fingerprint must not collide with --key-file");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/flag/key")),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn empty_env_file_does_not_collide_with_a_fingerprint_flag() {
        let source = resolve_key_source(
            Some("SHA256:flag".to_string()),
            None,
            None,
            Some(String::new()),
        )
        .expect("an empty env file must not collide with --key-fingerprint");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:flag"),
            other => panic!("expected AgentKey, got {other:?}"),
        }
    }

    #[test]
    fn both_env_vars_empty_defaults_to_agent() {
        let source = resolve_key_source(None, None, Some(String::new()), Some(String::new()))
            .expect("no error");
        assert!(matches!(source, KeySource::Agent));
    }

    /// The app's stricter behavior, taken as the shared one: a flag given as
    /// the empty string is an error up front, even before it would collide
    /// with anything.
    #[test]
    fn an_empty_fingerprint_flag_is_rejected_up_front() {
        let err = resolve_key_source(Some(String::new()), None, None, None)
            .expect_err("an empty --key-fingerprint must be refused");
        assert_eq!(err, KeySelectError::EmptyFingerprint);
    }

    #[test]
    fn an_empty_file_flag_is_rejected_up_front() {
        let err = resolve_key_source(None, Some(PathBuf::new()), None, None)
            .expect_err("an empty --key-file must be refused");
        assert_eq!(err, KeySelectError::EmptyFile);
    }

    /// An empty flag is refused even when its own env var would otherwise
    /// resolve fine — the flag is a deliberate argument and is checked
    /// before precedence is applied, not silently overridden by env.
    #[test]
    fn an_empty_fingerprint_flag_is_rejected_even_with_a_usable_env_var() {
        let err = resolve_key_source(Some(String::new()), None, Some("SHA256:env".into()), None)
            .expect_err("the empty flag must be refused, not shadowed by its env var");
        assert_eq!(err, KeySelectError::EmptyFingerprint);
    }

    #[cfg(feature = "cli")]
    mod key_args {
        use super::*;
        use clap::Parser;

        #[derive(clap::Parser, Debug)]
        struct Cli {
            #[command(flatten)]
            key: KeyArgs,
        }

        #[test]
        fn flattens_into_a_clap_parser() {
            let cli = Cli::try_parse_from([
                "bin",
                "--key-fingerprint",
                "SHA256:abc",
            ])
            .expect("KeyArgs must flatten into a clap::Parser");
            assert_eq!(cli.key.key_fingerprint.as_deref(), Some("SHA256:abc"));
            assert!(cli.key.key_file.is_none());
        }

        #[test]
        fn cli_is_well_formed() {
            use clap::CommandFactory;
            Cli::command().debug_assert();
        }
    }
}
