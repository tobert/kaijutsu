//! Where a configured secret may come from, so config files carry no key
//! material of their own.
//!
//! Two sources: a host file whose trimmed contents are the value, and a named
//! environment variable. Running a command to produce a value is deliberately
//! absent — host process execution has one owner, and a secret-fetching exec
//! site is a design decision rather than a helper. See `docs/issues.md`.
//!
//! Both sources fail loudly, and a value that resolves to empty is a failure
//! rather than an empty string: a process launched with a blank credential
//! fails somewhere far away from the mistake that caused it.
//!
//! Error strings are safe to log and to show a player — they name the source
//! (a path, a variable name), never the value read from it.

/// Read a secret from a host file: expand `~`, read, trim surrounding
/// whitespace. A file that is empty after trimming is an error.
pub fn read_secret_file(path: &str) -> Result<String, String> {
    let expanded = shellexpand::tilde(path);
    let raw = std::fs::read_to_string(expanded.as_ref())
        .map_err(|e| format!("cannot read '{path}': {e}"))?;
    let value = raw.trim().to_string();
    if value.is_empty() {
        return Err(format!("'{path}' is empty after trimming"));
    }
    Ok(value)
}

/// Read a secret from a named environment variable, trimmed.
///
/// Unset and empty are both errors. Naming a variable is a statement about
/// where the value lives, so a variable that holds nothing is a mistake to
/// report, not a value to pass on.
pub fn read_secret_env(var: &str) -> Result<String, String> {
    let raw =
        std::env::var(var).map_err(|_| format!("environment variable '{var}' is not set"))?;
    let value = raw.trim().to_string();
    if value.is_empty() {
        return Err(format!("environment variable '{var}' is set but empty"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_contents_are_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "  sk-from-file\n").unwrap();
        assert_eq!(read_secret_file(path.to_str().unwrap()).unwrap(), "sk-from-file");
    }

    #[test]
    fn a_whitespace_only_file_is_not_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank");
        std::fs::write(&path, "   \n\n").unwrap();
        let err = read_secret_file(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("empty after trimming"), "{err}");
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let err = read_secret_file("/nonexistent/path/to/token").unwrap_err();
        assert!(err.contains("/nonexistent/path/to/token"), "{err}");
    }

    /// The whole point of a file source is that the value never lands in a
    /// log line. Every error here is about the *source*, so the contents must
    /// not appear even when reading succeeded and validation then failed.
    #[test]
    fn an_error_never_carries_the_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank");
        std::fs::write(&path, "  \n").unwrap();
        let err = read_secret_file(path.to_str().unwrap()).unwrap_err();
        assert!(!err.contains('\n'), "error quoted raw file bytes: {err}");
    }

    #[test]
    fn env_var_is_read_and_trimmed() {
        // SAFETY: single-threaded test; unique var name avoids cross-test races.
        unsafe {
            std::env::set_var("KAIJUTSU_SECRET_SOURCE_TEST_SET", "  sk-from-env \n");
        }
        assert_eq!(read_secret_env("KAIJUTSU_SECRET_SOURCE_TEST_SET").unwrap(), "sk-from-env");
        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("KAIJUTSU_SECRET_SOURCE_TEST_SET");
        }
    }

    #[test]
    fn an_unset_env_var_is_an_error_not_an_empty_value() {
        let err = read_secret_env("KAIJUTSU_SECRET_SOURCE_TEST_DEFINITELY_UNSET").unwrap_err();
        assert!(err.contains("is not set"), "{err}");
    }

    #[test]
    fn an_empty_env_var_is_an_error() {
        // SAFETY: single-threaded test; unique var name.
        unsafe {
            std::env::set_var("KAIJUTSU_SECRET_SOURCE_TEST_BLANK", "   ");
        }
        let err = read_secret_env("KAIJUTSU_SECRET_SOURCE_TEST_BLANK").unwrap_err();
        assert!(err.contains("set but empty"), "{err}");
        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("KAIJUTSU_SECRET_SOURCE_TEST_BLANK");
        }
    }
}
