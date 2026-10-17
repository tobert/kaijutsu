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
/// The names are the ones a person says. Each maps onto the backend row a
/// solo kernel writes for it ([`ProviderRow`]): a kernel ships no backends.
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

/// The backend row a solo kernel writes for one provider.
struct ProviderRow {
    /// The backend row's name, which is also what `kj model` prints.
    backend: &'static str,
    /// The row's kind, which selects the wire the kernel speaks.
    kind: &'static str,
    /// Where the kernel looks for the key.
    api_key_env: Option<&'static str>,
    api_key_file: Option<&'static str>,
    /// The model a solo kernel uses when nobody names one. `None` means
    /// there is no model id we can stand behind, so `--model` is required.
    default_model: Option<&'static str>,
}

impl BackendKind {
    fn row(self) -> ProviderRow {
        match self {
            Self::Anthropic => ProviderRow {
                backend: "anthropic",
                kind: "anthropic",
                api_key_env: Some("ANTHROPIC_API_KEY"),
                api_key_file: Some("~/.anthropic-key.txt"),
                default_model: Some("claude-sonnet-5-5"),
            },
            Self::Deepseek => ProviderRow {
                backend: "deepseek",
                kind: "deepseek",
                api_key_env: Some("DEEPSEEK_API_KEY"),
                api_key_file: Some("~/.deepseek-key"),
                default_model: Some("deepseek-flash"),
            },
            Self::Openai => ProviderRow {
                backend: "gpt",
                kind: "openai",
                api_key_env: Some("OPENAI_API_KEY"),
                api_key_file: Some("~/.openai-key.txt"),
                // No model id for this backend has a measured window, so
                // there is nothing here to default to.
                default_model: None,
            },
            #[cfg(feature = "test-mock")]
            Self::Mock => ProviderRow {
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

/// The model row a solo kernel writes for a model it knows: the context
/// window and, for some models, the output budget and effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownModel {
    pub context_window: u64,
    pub max_tokens: Option<i64>,
    pub effort: Option<&'static str>,
}

const ANTHROPIC_200K: KnownModel = KnownModel { context_window: 200_000, max_tokens: None, effort: None };
const ANTHROPIC_1M: KnownModel = KnownModel { context_window: 1_000_000, max_tokens: None, effort: None };

/// DeepSeek V4 at effort `high` with a 64K output budget, the API's own
/// default at that effort. Reasoning tokens count against it.
const DEEPSEEK_V4: KnownModel = KnownModel { context_window: 1_000_000, max_tokens: Some(65536), effort: Some("high") };

/// `(backend, model, row)`. Windows were read from Anthropic's
/// `GET /v1/models/{id}` (`max_input_tokens`) and from api-docs.deepseek.com.
/// A model missing here gets no model row, so its window stays unknown.
const KNOWN_MODELS: &[(&str, &str, KnownModel)] = &[
    ("anthropic", "claude-haiku-4-5", ANTHROPIC_200K),
    ("anthropic", "claude-opus-4-5", ANTHROPIC_200K),
    ("anthropic", "claude-opus-4-7", ANTHROPIC_1M),
    ("anthropic", "claude-opus-4-8", ANTHROPIC_1M),
    ("anthropic", "claude-opus-5", ANTHROPIC_1M),
    ("anthropic", "claude-opus-5-5", ANTHROPIC_1M),
    ("anthropic", "claude-sonnet-4-6", ANTHROPIC_1M),
    ("anthropic", "claude-sonnet-5", ANTHROPIC_1M),
    ("anthropic", "claude-sonnet-5-5", ANTHROPIC_1M),
    ("anthropic", "claude-fable-5", ANTHROPIC_1M),
    ("anthropic", "claude-fable-5-1", ANTHROPIC_1M),
    ("deepseek", "deepseek-flash", DEEPSEEK_V4),
    ("deepseek", "deepseek-v4-pro", DEEPSEEK_V4),
];

/// The model row a solo kernel writes for `model` on `backend`, when it
/// knows the model.
pub fn known_model(backend: &str, model: &str) -> Option<KnownModel> {
    KNOWN_MODELS
        .iter()
        .find(|(b, m, _)| *b == backend && *m == model)
        .map(|(_, _, row)| *row)
}

/// What the solo kernel is told to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    pub backend: String,
    pub kind: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// The provider's key file, kept when the caller named no key variable.
    pub api_key_file: Option<String>,
    pub key_optional: bool,
    /// True when the flags name an endpoint or key of their own, so a backend
    /// row a reused state directory already holds is replaced. A state
    /// directory with no row for this backend always gets one.
    pub replace_backend_row: bool,
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
    let row = kind.row();

    let model = match flags.model.clone().or(row.default_model.map(Into::into)) {
        Some(model) => model,
        None => bail!(
            "--backend-kind {} needs --model <id>: this kernel ships no model id for the \
             {} backend",
            format!("{kind:?}").to_lowercase(),
            row.backend,
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
            .or(row.api_key_env.map(Into::into))
    };
    let key_optional = flags.no_key || row.api_key_env.is_none();

    // Say now that the key is missing. The alternative is a kernel that
    // starts, accepts a prompt, and fails the first turn.
    if !key_optional {
        let named = api_key_env.as_deref().unwrap_or_default();
        let have_env = host.var(named).is_some();
        let have_file = flags.api_key_env.is_none()
            && row
                .api_key_file
                .is_some_and(|path| host.key_file_exists(path));
        if !have_env && !have_file {
            match row.api_key_file {
                Some(file) if flags.api_key_env.is_none() => bail!(
                    "the {} backend has no key: set {named} or write the key to {file}",
                    row.backend,
                ),
                _ => bail!("{named} is unset, so the {} backend has no key", row.backend),
            }
        }
    }

    // A caller-supplied endpoint or key variable may not be what a row the
    // state directory already holds says, so that row is replaced. Otherwise
    // an existing row stands, with any edit made to it through `kj`.
    let replace_backend_row = flags.base_url.is_some() || flags.api_key_env.is_some() || flags.no_key;

    Ok(ModelChoice {
        backend: row.backend.to_string(),
        kind: row.kind.to_string(),
        model,
        base_url: flags.base_url.clone(),
        api_key_env,
        api_key_file: (!flags.no_key && flags.api_key_env.is_none())
            .then_some(row.api_key_file)
            .flatten()
            .map(Into::into),
        key_optional,
        replace_backend_row,
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
    /// Every default model has a known model row, so a solo kernel's
    /// default turn has a window and DeepSeek's has its 64K budget.
    #[test]
    fn each_default_model_has_a_known_row() {
        for kind in [BackendKind::Anthropic, BackendKind::Deepseek] {
            let row = kind.row();
            let model = row.default_model.expect("a default model");
            assert!(known_model(row.backend, model).is_some(), "{model}");
        }
        assert_eq!(known_model("deepseek", "deepseek-flash").unwrap().max_tokens, Some(65536));
        assert_eq!(known_model("gpt", "deepseek-flash"), None, "a row is per backend");
    }

    #[test]
    fn anthropic_defaults_to_the_current_sonnet() {
        assert_eq!(BackendKind::Anthropic.row().default_model, Some("claude-sonnet-5-5"));
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
        assert!(!choice.replace_backend_row, "no flag says anything of its own");
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
    fn openai_needs_a_model_because_there_is_no_default() {
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
        assert!(choice.replace_backend_row);
        assert_eq!(choice.api_key_env.as_deref(), Some("MY_KEY"));
        assert_eq!(choice.base_url.as_deref(), Some("http://localhost:8080/v1"));
    }

    #[test]
    fn an_endpoint_of_our_own_keeps_the_providers_key_file() {
        let host = FakeHost::default().with_key_file("~/.openai-key.txt");
        let flags = ModelFlags {
            backend_kind: Some(BackendKind::Openai),
            base_url: Some("https://example.test/v1".to_string()),
            model: Some("qwen3.8-flash".to_string()),
            ..ModelFlags::default()
        };
        let choice = resolve(&flags, &host).expect("the key file counts");
        assert!(choice.replace_backend_row);
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
        assert!(choice.replace_backend_row);
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
        // The provider's key file must not rescue a variable the caller named.
        let host = FakeHost::default().with_key_file("~/.deepseek-key");
        let error = resolve(&flags, &host).expect_err("the named variable is unset");
        assert!(error.to_string().contains("SOMEWHERE_ELSE"), "{error}");
    }
}
