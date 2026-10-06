//! Which provider a solo kernel talks to, and with what model.
//!
//! Resolution is explicit or it refuses. With `--backend-kind` the named
//! provider is used; with no flags at all, exactly one provider key in the
//! environment selects that provider. Zero keys, or several, is a refusal
//! that names what to set — a guess here would send a stranger's first
//! prompt to a provider they did not choose.

use anyhow::{Result, bail};

/// A provider this binary can point a solo kernel at.
///
/// The names are the ones a person says. Each maps onto a factory backend
/// row the kernel already ships (`kaijutsu_kernel::seed_backends`), so a
/// solo kernel reads its key exactly the way every other kernel does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum BackendKind {
    Anthropic,
    Deepseek,
    Openai,
    /// The scripted test backend, present only in a `test-mock` build.
    #[cfg(feature = "test-mock")]
    Mock,
}

/// What the factory floor knows about one provider.
struct Factory {
    /// The backend row's name, which is also what `kj model` prints.
    backend: &'static str,
    /// The row's kind, which selects the wire the kernel speaks.
    kind: &'static str,
    /// Where the kernel looks for the key.
    api_key_env: Option<&'static str>,
    api_key_file: Option<&'static str>,
    /// The model a solo kernel uses when nobody names one. `None` means the
    /// floor ships no model id we can stand behind, so `--model` is required.
    default_model: Option<&'static str>,
}

impl BackendKind {
    fn factory(self) -> Factory {
        match self {
            Self::Anthropic => Factory {
                backend: "anthropic",
                kind: "anthropic",
                api_key_env: Some("ANTHROPIC_API_KEY"),
                api_key_file: Some("~/.anthropic-key.txt"),
                default_model: Some("claude-sonnet-5-5"),
            },
            Self::Deepseek => Factory {
                backend: "deepseek",
                kind: "deepseek",
                api_key_env: Some("DEEPSEEK_API_KEY"),
                api_key_file: Some("~/.deepseek-key"),
                default_model: Some("deepseek-flash"),
            },
            Self::Openai => Factory {
                backend: "gpt",
                kind: "openai",
                api_key_env: Some("OPENAI_API_KEY"),
                api_key_file: Some("~/.openai-key.txt"),
                // The factory floor ships no model ids for this backend, so
                // there is nothing here to default to.
                default_model: None,
            },
            #[cfg(feature = "test-mock")]
            Self::Mock => Factory {
                backend: "mock",
                kind: "mock",
                api_key_env: None,
                api_key_file: None,
                default_model: None,
            },
        }
    }
}

/// The provider key environment variables, in the order a refusal lists
/// them.
const KEY_VARS: &[(&str, BackendKind)] = &[
    ("DEEPSEEK_API_KEY", BackendKind::Deepseek),
    ("ANTHROPIC_API_KEY", BackendKind::Anthropic),
    ("OPENAI_API_KEY", BackendKind::Openai),
];

/// What the solo kernel is told to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    pub backend: String,
    pub kind: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// The factory key file, kept when the caller named no key variable.
    pub api_key_file: Option<String>,
    pub key_optional: bool,
    /// True when this choice needs its own backend row written. False leaves
    /// the factory row alone, which is what keeps a provider's key file
    /// working.
    pub write_backend_row: bool,
    /// The backend row's idle timeout. `None` keeps the kernel default.
    pub idle_timeout_secs: Option<u64>,
    /// The backend row's request timeout. `None` keeps the kernel default.
    pub request_timeout_secs: Option<u64>,
}

/// What resolution reads about the world. Injected so the rules can be
/// tested without touching the environment of the test process.
pub trait Host {
    fn var(&self, name: &str) -> Option<String>;
    /// Whether a `~`-prefixed key file exists.
    fn key_file_exists(&self, path: &str) -> bool;
}

/// The real environment.
pub struct RealHost;

impl Host for RealHost {
    fn var(&self, name: &str) -> Option<String> {
        match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => None,
        }
    }

    fn key_file_exists(&self, path: &str) -> bool {
        let resolved = match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
            (Some(_), None) => return false,
            (None, _) => std::path::PathBuf::from(path),
        };
        resolved.is_file()
    }
}

/// The flags that decide the model.
#[derive(Clone, Debug, Default)]
pub struct ModelFlags {
    pub backend_kind: Option<BackendKind>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    /// The endpoint takes no key: a local server of our own.
    pub no_key: bool,
    /// Seconds a stream may go silent before the kernel gives up on it.
    pub idle_timeout_secs: Option<u64>,
    /// Seconds one whole completion may take.
    pub request_timeout_secs: Option<u64>,
}

/// Resolve the provider and model, or refuse with a message naming what to
/// set.
pub fn resolve(flags: &ModelFlags, host: &dyn Host) -> Result<ModelChoice> {
    let kind = match flags.backend_kind {
        Some(kind) => kind,
        None => autodetect(host)?,
    };
    let factory = kind.factory();

    let model = match flags.model.clone().or(factory.default_model.map(Into::into)) {
        Some(model) => model,
        None => bail!(
            "--backend-kind {} needs --model <id>: this kernel ships no model id for the \
             {} backend",
            format!("{kind:?}").to_lowercase(),
            factory.backend,
        ),
    };

    if flags.no_key {
        if flags.base_url.is_none() {
            bail!("--no-key needs --base-url: a provider's own endpoint always takes a key");
        }
        if flags.api_key_env.is_some() {
            bail!("--no-key and --api-key-env contradict each other; pass one");
        }
    }

    let api_key_env = if flags.no_key {
        None
    } else {
        flags
            .api_key_env
            .clone()
            .or(factory.api_key_env.map(Into::into))
    };
    let key_optional = flags.no_key || factory.api_key_env.is_none();

    // Say now that the key is missing. The alternative is a kernel that
    // starts, accepts a prompt, and fails the first turn.
    if !key_optional {
        let named = api_key_env.as_deref().unwrap_or_default();
        let have_env = host.var(named).is_some();
        let have_file = flags.api_key_env.is_none()
            && factory
                .api_key_file
                .is_some_and(|path| host.key_file_exists(path));
        if !have_env && !have_file {
            match factory.api_key_file {
                Some(file) if flags.api_key_env.is_none() => bail!(
                    "the {} backend has no key: set {named} or write the key to {file}",
                    factory.backend,
                ),
                _ => bail!("{named} is unset, so the {} backend has no key", factory.backend),
            }
        }
    }

    // A caller-supplied endpoint or key variable is not what the factory row
    // says, so the row has to be written. Otherwise leave it alone: the
    // factory row also names the provider's key FILE, and rewriting it here
    // would drop that fallback.
    let write_backend_row = flags.base_url.is_some()
        || flags.api_key_env.is_some()
        || flags.no_key
        || !kaijutsu_kernel::seed_backends::is_factory_backend_name(factory.backend);

    Ok(ModelChoice {
        backend: factory.backend.to_string(),
        kind: factory.kind.to_string(),
        model,
        base_url: flags.base_url.clone(),
        api_key_env,
        api_key_file: (!flags.no_key && flags.api_key_env.is_none())
            .then_some(factory.api_key_file)
            .flatten()
            .map(Into::into),
        key_optional,
        write_backend_row,
        idle_timeout_secs: flags.idle_timeout_secs,
        request_timeout_secs: flags.request_timeout_secs,
    })
}

/// Exactly one provider key in the environment selects that provider.
fn autodetect(host: &dyn Host) -> Result<BackendKind> {
    let present: Vec<&(&str, BackendKind)> = KEY_VARS
        .iter()
        .filter(|(var, _)| host.var(var).is_some())
        .collect();
    match present.as_slice() {
        [(_, kind)] => Ok(*kind),
        [] => bail!(
            "no model configured. Set one of {}, or name a provider with \
             --backend-kind <anthropic|deepseek|openai> --model <id>",
            KEY_VARS
                .iter()
                .map(|(var, _)| *var)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        several => bail!(
            "{} are all set, so the provider is ambiguous. Name one with --backend-kind",
            several
                .iter()
                .map(|(var, _)| *var)
                .collect::<Vec<_>>()
                .join(" and "),
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn anthropic_defaults_to_the_current_sonnet() {
        assert_eq!(BackendKind::Anthropic.factory().default_model, Some("claude-sonnet-5-5"));
    }

    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeHost {
        vars: BTreeMap<String, String>,
        files: Vec<String>,
    }

    impl FakeHost {
        fn with_var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.to_string(), value.to_string());
            self
        }

        fn with_key_file(mut self, path: &str) -> Self {
            self.files.push(path.to_string());
            self
        }
    }

    impl Host for FakeHost {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }

        fn key_file_exists(&self, path: &str) -> bool {
            self.files.iter().any(|known| known == path)
        }
    }

    #[test]
    fn one_key_in_the_environment_picks_that_provider() {
        let host = FakeHost::default().with_var("DEEPSEEK_API_KEY", "sk-x");
        let choice = resolve(&ModelFlags::default(), &host).expect("one key resolves");
        assert_eq!(choice.backend, "deepseek");
        assert_eq!(choice.model, "deepseek-flash");
        assert!(!choice.write_backend_row, "the factory row already says this");
    }

    #[test]
    fn no_key_names_every_variable_it_would_accept() {
        let error = resolve(&ModelFlags::default(), &FakeHost::default())
            .expect_err("nothing configured must refuse");
        let message = error.to_string();
        for var in ["DEEPSEEK_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
            assert!(message.contains(var), "{var} missing from {message}");
        }
    }

    #[test]
    fn two_keys_refuse_rather_than_pick_one() {
        let host = FakeHost::default()
            .with_var("DEEPSEEK_API_KEY", "sk-x")
            .with_var("ANTHROPIC_API_KEY", "sk-y");
        let error = resolve(&ModelFlags::default(), &host).expect_err("ambiguous must refuse");
        let message = error.to_string();
        assert!(message.contains("DEEPSEEK_API_KEY") && message.contains("ANTHROPIC_API_KEY"));
        assert!(message.contains("--backend-kind"), "{message}");
    }

    #[test]
    fn a_key_file_is_enough_for_a_named_provider() {
        let host = FakeHost::default().with_key_file("~/.deepseek-key");
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Deepseek),
            ..ModelFlags::default()
        };
        let choice = resolve(&flags, &host).expect("the key file counts");
        assert_eq!(choice.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
    }

    #[test]
    fn a_named_provider_with_no_key_anywhere_is_refused() {
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Anthropic),
            ..ModelFlags::default()
        };
        let error = resolve(&flags, &FakeHost::default()).expect_err("no key must refuse");
        let message = error.to_string();
        assert!(message.contains("ANTHROPIC_API_KEY"), "{message}");
        assert!(message.contains("~/.anthropic-key.txt"), "{message}");
    }

    #[test]
    fn openai_needs_a_model_because_the_floor_ships_none() {
        let host = FakeHost::default().with_var("OPENAI_API_KEY", "sk-x");
        let error = resolve(&ModelFlags::default(), &host).expect_err("no model id to default to");
        assert!(error.to_string().contains("--model"), "{error}");

        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            model: Some("gpt-5.6".to_string()),
            ..ModelFlags::default()
        };
        let choice = resolve(&flags, &host).expect("a named model resolves");
        assert_eq!(choice.backend, "gpt");
        assert_eq!(choice.kind, "openai");
    }

    #[test]
    fn an_endpoint_or_key_variable_of_our_own_writes_the_row() {
        let host = FakeHost::default().with_var("MY_KEY", "sk-x");
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            base_url: Some("http://localhost:8080/v1".to_string()),
            model: Some("local-model".to_string()),
            api_key_env: Some("MY_KEY".to_string()),
            ..ModelFlags::default()
        };
        let choice = resolve(&flags, &host).expect("a local endpoint resolves");
        assert!(choice.write_backend_row);
        assert_eq!(choice.api_key_env.as_deref(), Some("MY_KEY"));
        assert_eq!(choice.base_url.as_deref(), Some("http://localhost:8080/v1"));
    }

    #[test]
    fn an_endpoint_of_our_own_keeps_the_factory_key_file() {
        let host = FakeHost::default().with_key_file("~/.openai-key.txt");
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            base_url: Some("https://example.test/v1".to_string()),
            model: Some("qwen3.8-flash".to_string()),
            ..ModelFlags::default()
        };
        let choice = resolve(&flags, &host).expect("the key file counts");
        assert!(choice.write_backend_row);
        assert_eq!(choice.api_key_file.as_deref(), Some("~/.openai-key.txt"),
            "the row written must keep the key file resolution accepted");
    }

    #[test]
    fn no_key_needs_no_key_and_writes_a_keyless_row() {
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            base_url: Some("http://tenchi:8000/v1".to_string()),
            model: Some("qwen3.8-27b".to_string()),
            no_key: true,
            ..ModelFlags::default()
        };
        let choice = resolve(&flags, &FakeHost::default()).expect("a keyless endpoint resolves");
        assert!(choice.key_optional);
        assert_eq!(choice.api_key_env, None, "no key variable to read");
        assert!(choice.write_backend_row);
    }

    #[test]
    fn no_key_without_an_endpoint_is_refused() {
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            model: Some("gpt-5.6".to_string()),
            no_key: true,
            ..ModelFlags::default()
        };
        let error = resolve(&flags, &FakeHost::default()).expect_err("a hosted endpoint needs a key");
        assert!(error.to_string().contains("--base-url"), "{error}");
    }

    #[test]
    fn no_key_with_a_key_variable_is_refused() {
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            base_url: Some("http://tenchi:8000/v1".to_string()),
            model: Some("qwen3.8-27b".to_string()),
            api_key_env: Some("MY_KEY".to_string()),
            no_key: true,
            ..ModelFlags::default()
        };
        let error = resolve(&flags, &FakeHost::default().with_var("MY_KEY", "x"))
            .expect_err("both is a contradiction");
        assert!(error.to_string().contains("--api-key-env"), "{error}");
    }

    #[test]
    fn a_named_key_variable_that_is_unset_is_refused() {
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Deepseek),
            api_key_env: Some("SOMEWHERE_ELSE".to_string()),
            ..ModelFlags::default()
        };
        // The factory key file must not rescue a variable the caller named.
        let host = FakeHost::default().with_key_file("~/.deepseek-key");
        let error = resolve(&flags, &host).expect_err("the named variable is unset");
        assert!(error.to_string().contains("SOMEWHERE_ELSE"), "{error}");
    }
}
