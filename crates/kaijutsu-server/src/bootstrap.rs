//! `kaijutsu-server bootstrap`: set up a kernel so its console can
//! answer (`docs/server-cli.md`, "bootstrap").
//!
//! The root character and its key are written directly, as `init` writes
//! them. Everything else is a `kj` verb, dispatched as that root on a kernel
//! booted offline, so bootstrap does what a person typing in their root would
//! do. Each verb runs only when what it makes is missing, so a second run
//! changes nothing, and a kernel set up by hand is completed rather than
//! overwritten.

use kaijutsu_kernel::kernel_db::{KernelDb, KernelDbError, KernelDbResult};
use kaijutsu_kernel::{KjCaller, KjResult};
use russh::keys::ssh_key::PublicKey;

use crate::auth_db::AuthDb;
use crate::offline::{KernelLock, OFFLINE_HOLD_REFUSAL, resolve_caller, resolve_data_dir, settle};
use crate::rpc::SharedKernel;
use crate::ssh::SshServerConfig;

/// The label of the model console.
const CONSOLE_LABEL: &str = "console";
/// The response budget bootstrap gives the performer's model.
const DEFAULT_MAX_TOKENS: u64 = 16384;


/// One backend row, and the model the kernel defaults to on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendChoice {
    /// The backend row's name, as `kj backend list` prints it.
    pub name: String,
    /// The wire dialect (`kj backend set --kind`).
    pub kind: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub key_optional: bool,
}

impl BackendChoice {
    /// The scripted test backend: it needs no key, and with no script it
    /// answers every prompt with one canned sentence.
    pub fn mock(model: &str) -> Self {
        Self {
            name: "mock".to_string(),
            kind: "mock".to_string(),
            model: model.to_string(),
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
        }
    }

    /// A provider bootstrap knows by name, with its usual key locations and
    /// its default model. `None` for a kind it does not know.
    pub fn provider(kind: &str) -> Option<Self> {
        let provider = PROVIDERS.iter().find(|p| p.kind == kind)?;
        Some(Self {
            name: provider.kind.to_string(),
            kind: provider.kind.to_string(),
            model: provider.default_model.to_string(),
            base_url: None,
            api_key_env: Some(provider.api_key_env.to_string()),
            api_key_file: Some(provider.api_key_file.to_string()),
            key_optional: false,
        })
    }

    fn set_argv(&self) -> Vec<String> {
        let mut argv = words(&["backend", "set", &self.name, "--kind", &self.kind]);
        let optional = [
            ("--base-url", &self.base_url),
            ("--api-key-env", &self.api_key_env),
            ("--api-key-file", &self.api_key_file),
        ];
        for (flag, value) in optional {
            if let Some(value) = value {
                argv.extend(words(&[flag, value]));
            }
        }
        if self.key_optional {
            argv.push("--key-optional".to_string());
        }
        argv
    }
}

/// A provider whose key bootstrap looks for.
struct Provider {
    kind: &'static str,
    api_key_env: &'static str,
    api_key_file: &'static str,
    default_model: &'static str,
}

/// In order of preference: the first one with a key becomes the default.
const PROVIDERS: &[Provider] = &[
    Provider {
        kind: "anthropic",
        api_key_env: "ANTHROPIC_API_KEY",
        api_key_file: "~/.anthropic-key.txt",
        default_model: "claude-sonnet-5-5",
    },
    Provider {
        kind: "deepseek",
        api_key_env: "DEEPSEEK_API_KEY",
        api_key_file: "~/.deepseek-key",
        default_model: "deepseek-flash",
    },
];

/// Every provider with a key in the environment or its usual key file, in
/// order of preference.
pub fn detect_backends(var: impl Fn(&str) -> bool, file_exists: impl Fn(&str) -> bool) -> Vec<BackendChoice> {
    PROVIDERS
        .iter()
        .filter(|p| var(p.api_key_env) || file_exists(p.api_key_file))
        .filter_map(|p| BackendChoice::provider(p.kind))
        .collect()
}

/// The metadata bootstrap writes for a model it knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KnownModel {
    context_window: u64,
    max_tokens: Option<u64>,
    effort: Option<&'static str>,
}

const ANTHROPIC_200K: KnownModel = KnownModel { context_window: 200_000, max_tokens: None, effort: None };
const ANTHROPIC_1M: KnownModel = KnownModel { context_window: 1_000_000, max_tokens: None, effort: None };

/// DeepSeek V4 at effort `high` with a 64K output budget, the API's own
/// default at that effort. Reasoning tokens count against it.
const DEEPSEEK_V4: KnownModel = KnownModel { context_window: 1_000_000, max_tokens: Some(65536), effort: Some("high") };

/// `(kind, model, metadata)`. Windows were read from Anthropic's
/// `GET /v1/models/{id}` (`max_input_tokens`) and from api-docs.deepseek.com.
/// A model missing here gets no model row, so its window stays unknown.
const KNOWN_MODELS: &[(&str, &str, KnownModel)] = &[
    ("anthropic", "claude-haiku-4-5", ANTHROPIC_200K),
    ("anthropic", "claude-haiku-5-5", ANTHROPIC_1M),
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

fn known_model(kind: &str, model: &str) -> Option<KnownModel> {
    KNOWN_MODELS.iter().find(|(k, m, _)| *k == kind && *m == model).map(|(_, _, row)| *row)
}

/// What to set up.
#[derive(Clone, Debug)]
pub struct BootstrapOptions {
    /// The root character: the person who runs this kernel.
    pub root: String,
    pub root_key: PublicKey,
    pub key_comment: Option<String>,
    /// The backends to register. The first becomes the kernel default when
    /// the kernel has none. Empty leaves the console waiting for a model.
    pub backends: Vec<BackendChoice>,
    /// The model character that plays the console.
    pub console_performer: String,
}

/// What a bootstrap did.
#[derive(Clone, Debug, Default)]
pub struct BootstrapReport {
    /// Each `kj` line that ran, in order.
    pub ran: Vec<String>,
    /// What the person should know: what was already there, and what is
    /// still missing.
    pub notes: Vec<String>,
}

/// Set up the kernel at `config`'s paths. The kernel must not be serving:
/// this takes the same lock the service takes.
pub async fn bootstrap(config: &SshServerConfig, opts: &BootstrapOptions) -> Result<BootstrapReport, String> {
    if opts.console_performer == opts.root {
        return Err(format!(
            "the root character and the console's performer are both '{}'; a model turn needs a performer \
             who is not its own reviewer, so name the performer with --console-as",
            opts.root
        ));
    }

    let data_dir = resolve_data_dir(config.data_dir.as_deref());
    let _lock = KernelLock::acquire(&data_dir)?;

    {
        let kernel_db = KernelDb::open(data_dir.join("kernel.db"))
            .map_err(|e| format!("open {}: {e}", data_dir.join("kernel.db").display()))?;
        let auth_path = config.auth_db_path.clone().unwrap_or_else(AuthDb::default_path);
        let auth_db = AuthDb::open(&auth_path).map_err(|e| format!("open {}: {e}", auth_path.display()))?;
        crate::init::init_root(&kernel_db, &auth_db, &opts.root, &opts.root_key, opts.key_comment.as_deref())?;
    }

    let shared = crate::rpc::create_shared_kernel(
        config.config_dir.as_deref(),
        &config.config_mounts,
        config.data_dir.as_deref(),
        &config.rw_mounts,
        &config.unlisted,
    )
    .await
    .map_err(|e| format!("boot the kernel: {e}"))?;
    shared.kernel.refuse_holds(OFFLINE_HOLD_REFUSAL);

    let result = match resolve_caller(&shared, Some(&opts.root), None) {
        Ok(caller) => run_plan(&shared, &caller, opts).await,
        Err(e) => Err(e),
    };
    // Settle either way. A verb's refusal is the error to report; a settle
    // failure after it is added rather than put in its place.
    match (result, settle(&shared).await) {
        (Ok(report), Ok(())) => Ok(report),
        (Ok(_), Err(settled)) => Err(settled),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(settled)) => Err(format!("{e}; then {settled}")),
    }
}

/// Plan each missing piece from what the kernel holds now, and run it.
/// Planning reads the database between verbs, so a verb sees the rows the
/// one before it wrote.
async fn run_plan(shared: &SharedKernel, caller: &KjCaller, opts: &BootstrapOptions) -> Result<BootstrapReport, String> {
    let mut report = BootstrapReport::default();

    for backend in &opts.backends {
        let exists = read(shared, |db| db.get_backend_by_name(&backend.name))?.is_some();
        if exists {
            report.notes.push(format!(
                "backend '{}' is already set; `kj backend set {} …` changes it",
                backend.name, backend.name
            ));
        } else {
            dispatch(shared, caller, backend.set_argv(), &mut report).await?;
        }
        let row = read(shared, |db| db.get_backend_by_name(&backend.name))?
            .ok_or_else(|| format!("no '{}' backend row after setting it", backend.name))?;
        let models = read(shared, |db| db.list_backend_models(row.backend_id))?;
        if !models.iter().any(|m| m.model_id == backend.model) {
            let Some(known) = known_model(&backend.kind, &backend.model) else {
                report.notes.push(format!(
                    "bootstrap does not know the context window of {}/{}; `kj backend model set {} {} \
                     --context-window <tokens>` records it, and `kj context rotate console` gives the console its window",
                    backend.name, backend.model, backend.name, backend.model
                ));
                continue;
            };
            let mut argv = words(&[
                "backend",
                "model",
                "set",
                &backend.name,
                &backend.model,
                "--context-window",
                &known.context_window.to_string(),
            ]);
            if let Some(max_tokens) = known.max_tokens {
                argv.extend(words(&["--max-tokens", &max_tokens.to_string()]));
            }
            if let Some(effort) = known.effort {
                argv.extend(words(&["--effort", effort]));
            }
            dispatch(shared, caller, argv, &mut report).await?;
        }
    }

    let performer = &opts.console_performer;
    // A new performer is created with its model. An existing one with neither
    // a model nor a slot gets the model; one that has either is left alone.
    let first = opts.backends.first();
    match read(shared, |db| db.get_character_by_name(performer))? {
        None => {
            // The console's performer runs the console's rc directory.
            let mut argv = words(&["character", "create", performer, "--rc", "console"]);
            if let Some(backend) = first {
                argv.extend(words(&[
                    "--model", &format!("{}/{}", backend.name, backend.model), "--max-tokens",
                    &DEFAULT_MAX_TOKENS.to_string(),
                ]));
            }
            dispatch(shared, caller, argv, &mut report).await?;
        }
        Some(row) if row.retired_at.is_some() => {
            return Err(format!("'{performer}' is retired; name the console's performer with --console-as"));
        }
        Some(row) if row.root => {
            return Err(format!(
                "'{performer}' is a root character and cannot play the console; name the console's performer with --console-as"
            ));
        }
        Some(row) => {
            if row.rc_dir.as_deref() != Some("console") {
                // Every live context the performer plays but the root's console.
                let others: Vec<String> = read(shared, |db| {
                    let console = db.get_character_by_name(&opts.root)?.and_then(|sheet| sheet.console_ctx);
                    Ok(db.character_live_contexts(row.principal_id)?
                        .into_iter()
                        .filter(|id| Some(*id) != console)
                        .map(|id| db.context_name(id))
                        .collect())
                })?;
                let current = row.rc_dir.as_deref().unwrap_or("(none)");
                report.notes.push(if others.is_empty() {
                    format!(
                        "{performer} runs rc directory {current}, not console, so the console it plays runs that directory; \
                         `kj character set {performer} --rc console` changes it, and changes the directory of every context {performer} plays"
                    )
                } else {
                    format!(
                        "{performer} runs rc directory {current}, not console, so the console it plays runs that directory. \
                         It also plays {}, whose lifecycles would change with the directory, so bootstrap does not \
                         recommend changing the directory; name another performer with --console-as",
                        others.join(", ")
                    )
                });
            }
            let has_model = read(shared, |db| db.get_character_model(row.principal_id))?.is_some();
            if has_model {
                report.notes.push(format!("{performer} already has a model; `kj character model set` changes it"));
            } else if let Some(backend) = first {
                dispatch(
                    shared,
                    caller,
                    words(&[
                        "character", "model", "set", performer, &format!("{}/{}", backend.name, backend.model),
                        "--max-tokens", &DEFAULT_MAX_TOKENS.to_string(),
                    ]),
                    &mut report,
                )
                .await?;
            }
        }
    }
    if first.is_none() {
        report.notes.push(
            "The console has no model yet. Give the kernel a key and run bootstrap again, or set one \
             from your root: `kj backend set anthropic --kind anthropic --api-key-file \
             ~/.anthropic-key.txt`, then give the character a model with `kj character model set`"
                .to_string(),
        );
    }

    let played_id = read(shared, |db| db.get_character_by_name(performer))?
        .ok_or_else(|| format!("no '{performer}' character row"))?
        .principal_id;
    // The console is the live context the root's sheet points at. With no
    // pointer, a live context labeled `console` that the performer plays
    // under the root's root console is adopted; one that is not is left
    // alone and named.
    let (root_console, pointed) = read(shared, |db| {
        let sheet = db.get_character_by_name(&opts.root)?;
        let pointed = match sheet.as_ref().and_then(|sheet| sheet.console_ctx) {
            Some(id) => db.get_context(id)?.filter(|row| !row.is_archived()),
            None => None,
        };
        Ok((sheet.and_then(|sheet| sheet.root_ctx), pointed))
    })?;
    let root_console = root_console.ok_or_else(|| format!("'{}' has no root context for the console to answer to", opts.root))?;
    let console = match pointed {
        Some(row) => Some(row),
        None => match read(shared, |db| db.find_context_by_label(CONSOLE_LABEL))? {
            None => {
                let argv = words(&[
                    "context", "create", performer, "--name", CONSOLE_LABEL,
                    "--parent", &root_console.to_string(), "--senpai", &root_console.to_string(),
                ]);
                dispatch(shared, caller, argv, &mut report).await?;
                dispatch(shared, caller, words(&["character", "set", &opts.root, "--console", CONSOLE_LABEL]), &mut report).await?;
                None
            }
            Some(row) if row.performer == Some(played_id) && row.senpai_ctx == Some(root_console) => {
                dispatch(shared, caller, words(&["character", "set", &opts.root, "--console", CONSOLE_LABEL]), &mut report).await?;
                Some(row)
            }
            Some(row) => {
                let performed = match row.performer {
                    Some(other) => format!("performed by {}", read(shared, |db| Ok(db.name_for(other)))?),
                    None => "with no performer".to_string(),
                };
                report.notes.push(format!(
                    "{} holds the label {CONSOLE_LABEL} ({performed}) and is not {}'s console; archive or rename it \
                     and run bootstrap again to create the console",
                    row.context_id.short(), opts.root
                ));
                return Ok(report);
            }
        },
    };
    match console {
        // Just created, with the performer's model as it is now.
        None => {}
        Some(row) if row.performer != Some(played_id) => report.notes.push(format!(
            "The console is not played by {performer}; rotation keeps its character, so archive the console and run \
             bootstrap again to create it as {performer}"
        )),
        // A context copies its character's model when it is made, so a
        // console whose own model does not resolve takes its performer's on
        // rotation, once the performer has a chat model.
        Some(row) if read(shared, |db| Ok(performer_model_usable(db, played_id)? && !console_model_resolves(db, row.context_id)?))? => {
            let label = row.label.clone().unwrap_or_else(|| row.context_id.to_string());
            let argv = words(&["context", "rotate", &label]);
            let line = format!("kj {}", argv.join(" "));
            match shared.kj_dispatcher.dispatch(&argv, caller).await {
                KjResult::Ok { .. } | KjResult::Switch(..) => report.ran.push(line),
                KjResult::Err(message) | KjResult::Latch { message, .. } => report.notes.push(format!(
                    "The console has no model it can run, and the rotation that gives it {performer}'s refused: \
                     {message}. Resolve that, then run `{line}`"
                )),
            }
        }
        Some(_) => report.notes.push("The console is already set up".to_string()),
    }

    Ok(report)
}

/// Whether `performer` has a model a console can run: one on a backend that
/// is no council server.
fn performer_model_usable(db: &KernelDb, performer: kaijutsu_types::PrincipalId) -> KernelDbResult<bool> {
    match db.get_character_model(performer)? {
        Some(model) => Ok(!db.backend_is_council_server(model.backend_id)?),
        None => Ok(false),
    }
}

/// Whether the console's own `model` slot resolves to a chat model. A slot
/// on a council server, a chat slot with no `max_tokens`, and no slot at all
/// do not.
fn console_model_resolves(db: &KernelDb, console: kaijutsu_types::ContextId) -> KernelDbResult<bool> {
    match kaijutsu_kernel::context_model(db, console) {
        Ok(kaijutsu_kernel::ContextModelLookup::Model(_)) => Ok(true),
        Ok(kaijutsu_kernel::ContextModelLookup::None(_)) => Ok(false),
        Err(KernelDbError::Validation(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

fn read<T>(
    shared: &SharedKernel,
    f: impl FnOnce(&KernelDb) -> kaijutsu_kernel::kernel_db::KernelDbResult<T>,
) -> Result<T, String> {
    f(&shared.kernel_db.lock()).map_err(|e| format!("read kernel.db: {e}"))
}

/// Run one verb as the root. A refusal stops the bootstrap and names the
/// line that failed.
async fn dispatch(
    shared: &SharedKernel,
    caller: &KjCaller,
    argv: Vec<String>,
    report: &mut BootstrapReport,
) -> Result<(), String> {
    let line = format!("kj {}", argv.join(" "));
    match shared.kj_dispatcher.dispatch(&argv, caller).await {
        KjResult::Ok { .. } | KjResult::Switch(..) => {
            report.ran.push(line);
            Ok(())
        }
        KjResult::Err(message) => Err(format!("{line}: {message}")),
        KjResult::Latch { message, .. } => Err(format!("{line}: confirmation required ({message})")),
    }
}

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_finds_each_provider_with_a_key_in_preference_order() {
        let found = detect_backends(|var| var == "DEEPSEEK_API_KEY", |file| file == "~/.anthropic-key.txt");
        let kinds: Vec<&str> = found.iter().map(|b| b.kind.as_str()).collect();
        assert_eq!(kinds, ["anthropic", "deepseek"]);
        assert!(detect_backends(|_| false, |_| false).is_empty());
    }

    #[test]
    fn each_provider_default_model_has_known_metadata() {
        for provider in PROVIDERS {
            assert!(known_model(provider.kind, provider.default_model).is_some(), "{}", provider.default_model);
        }
    }

    #[test]
    fn a_provider_row_names_both_key_sources() {
        let argv = BackendChoice::provider("deepseek").unwrap().set_argv();
        assert_eq!(
            argv.join(" "),
            "backend set deepseek --kind deepseek --api-key-env DEEPSEEK_API_KEY --api-key-file ~/.deepseek-key"
        );
    }
}
