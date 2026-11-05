//! Create a context through `kj context create`.
//!
//! The kernel has no RPC for creating a context. A client runs `kj context
//! create` from an existing context, which becomes the new context's senpai
//! and, unless `--parent` names another, its `forked_from`, so every context
//! has a root above it (`docs/character.md`, "Bootstrap: the person creates
//! themself"). The kernel never chooses that context; a client that has no
//! context yet chooses one with [`choose_parent`]. An MCP session created
//! from a root console takes the MCP hub its person's sheet names as
//! `forked_from` ([`ActorHandle::mcp_hub_of`]).
//!
//! A client decides from the wire facts on [`ContextInfo`] (`performer`,
//! `is_root_console`, `is_mcp_session`), never from a label.
//!
//! A client that has decided to archive a context it created runs `kj
//! context archive <id> --confirm` from [`archive_runs_from`]
//! ([`ActorHandle::archive_context_from_parent`]).

use kaijutsu_types::ContextId;

use crate::actor::{ActorHandle, CallError};
use crate::rpc::{ContextInfo, KjExecutionResult};

/// Why [`choose_parent`] picked a context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentSource {
    /// The caller named it (`--parent`, `KAIJUTSU_PARENT`).
    Explicit,
    /// Nothing was named and the kernel has exactly one live root context.
    /// Callers log this choice.
    OnlyRoot,
}

/// The context a client runs `kj context create` from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentChoice {
    pub context_id: ContextId,
    pub label: String,
    pub source: ParentSource,
}

fn is_live(context: &ContextInfo) -> bool {
    !context.archived && context.concluded_at.is_none()
}

fn is_root_context(context: &ContextInfo) -> bool {
    context.is_root_console
}

/// Choose the parent for a new context from a `listContexts` result.
///
/// `explicit` names a live context by label or id and always wins. With no
/// name, the kernel's only live root context is chosen. No root, or more
/// than one, is an error that lists what was found.
pub fn choose_parent(explicit: Option<&str>, contexts: &[ContextInfo]) -> Result<ParentChoice, String> {
    if let Some(name) = explicit {
        let by_id = ContextId::parse(name).ok();
        return contexts
            .iter()
            .filter(|context| is_live(context))
            .find(|context| context.label == name || Some(context.id) == by_id)
            .map(|context| ParentChoice {
                context_id: context.id,
                label: context.label.clone(),
                source: ParentSource::Explicit,
            })
            .ok_or_else(|| format!("parent '{name}' names no live context"));
    }
    let roots: Vec<&ContextInfo> =
        contexts.iter().filter(|context| is_live(context) && is_root_context(context)).collect();
    match roots.as_slice() {
        [only] => Ok(ParentChoice {
            context_id: only.id,
            label: only.label.clone(),
            source: ParentSource::OnlyRoot,
        }),
        [] => Err("this kernel has no live root context; run `kaijutsu-server init` on its host".into()),
        many => Err(format!(
            "this kernel has {} root contexts ({}); name the parent with --parent <label>",
            many.len(),
            many.iter().map(|context| context.label.as_str()).collect::<Vec<_>>().join(", "),
        )),
    }
}

/// The MCP hub a session created from `parent` lands under: `Some` when
/// `parent` is a root console (the hub is the new context's `forked_from`,
/// the root console its senpai), `None` for any other parent. `hub` is the
/// `mcp_hub_ctx` of the person's sheet. A root console whose sheet names no
/// hub, or a hub that is not live in `contexts`, is an error.
pub fn live_mcp_hub(parent: &ContextInfo, hub: Option<ContextId>, contexts: &[ContextInfo]) -> Result<Option<ContextId>, String> {
    if !is_root_context(parent) {
        return Ok(None);
    }
    let person = parent.performer.as_deref().unwrap_or(&parent.label);
    let why = match hub {
        None => "its sheet names none".to_string(),
        Some(hub) => match contexts.iter().find(|context| context.id == hub) {
            Some(context) if is_live(context) => return Ok(Some(hub)),
            Some(_) => format!("its hub {} is archived or concluded", hub.short()),
            None => format!("its hub {} is not in the kernel's listing", hub.short()),
        },
    };
    Err(format!(
        "root console '{}' of {person} has no live MCP hub ({why}): the kernel makes one at start and at \
         `kj character create --root`; restart the kernel, or name a parent with --parent <label>",
        parent.label
    ))
}

/// The `kj` argv that creates `label`, performed by the live character
/// `performer`, with its shell in `cwd` when one is given. The create
/// lifecycle (the performer's rc directory) runs in that cwd.
pub fn context_create_argv(label: &str, performer: &str, cwd: Option<&str>) -> Vec<String> {
    context_create_argv_under(label, performer, cwd, None)
}

/// [`context_create_argv`] with `--parent <forked_from>` when one is given:
/// the new context's `forked_from`, which is not its senpai (the context the
/// create runs from). `forked_from` may be a label containing `/`.
pub fn context_create_argv_under(
    label: &str,
    performer: &str,
    cwd: Option<&str>,
    forked_from: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "context".to_string(), "create".to_string(),
        performer.to_string(),
        "--name".to_string(),
        label.to_string(),
    ];
    if let Some(cwd) = cwd {
        argv.push("--cwd".to_string());
        argv.push(cwd.to_string());
    }
    if let Some(forked_from) = forked_from {
        argv.push("--parent".to_string());
        argv.push(forked_from.to_string());
    }
    argv
}

/// The `kj` argv that archives `context_id` for a caller that has already
/// decided, so it confirms.
pub fn context_archive_argv(context_id: ContextId) -> Vec<String> {
    vec!["context".to_string(), "archive".to_string(), context_id.to_hex(), "--confirm".to_string()]
}

/// The context that runs the archive of `context_id`: its parent, the
/// context it was created from, which holds the `operator` capability a
/// context such as a coder's lacks. A context with no parent runs it itself,
/// and so does an MCP session: its parent is a hub with no loadout, and its
/// own character may archive it.
pub fn archive_runs_from(context_id: ContextId, contexts: &[ContextInfo]) -> Result<ContextId, String> {
    let context = contexts
        .iter()
        .find(|context| context.id == context_id)
        .ok_or_else(|| format!("no such context: {}", context_id.short()))?;
    if context.is_mcp_session {
        return Ok(context_id);
    }
    Ok(context.forked_from.unwrap_or(context_id))
}

/// The new context's id from a `kj context create` result. A nonzero exit
/// is an error carrying the kernel's message.
pub fn context_id_from_create_result(result: &KjExecutionResult) -> Result<ContextId, String> {
    if result.exit_code != 0 {
        let detail = if result.stderr.trim().is_empty() { &result.stdout } else { &result.stderr };
        return Err(detail.trim().to_string());
    }
    let id = result
        .data
        .as_ref()
        .and_then(|data| data.get("context_id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "kj context create succeeded without context_id data".to_string())?;
    ContextId::parse(id).map_err(|e| format!("kj context create returned an invalid context_id '{id}': {e}"))
}

/// Why [`ActorHandle::create_context_under`] failed.
#[derive(Debug)]
pub enum CreateContextError {
    /// The call never produced a `kj` result: connection, timeout, shutdown.
    Call(CallError),
    /// The kernel ran `kj context create` and refused, with its message.
    Refused(String),
    /// A `kj` read gave no answer to decide on. `kj character show` reports
    /// a missing character and a failed lookup the same untyped way, so this
    /// is not a refusal.
    Unread(String),
    /// A gate holds the create for its reviewer. When the reviewer allows
    /// it, the create runs with the arguments it was given, and the caller
    /// joins the new context on a later call. Only
    /// [`ActorHandle::create_context_in_client_cwd`] reports this.
    Held {
        /// The gate's refusal, which names the ask.
        refusal: kaijutsu_types::Refusal,
    },
}

impl CreateContextError {
    /// The kernel refused because another live context holds the label.
    pub fn is_label_conflict(&self) -> bool {
        matches!(self, Self::Refused(message) if message.contains("label conflict"))
    }

    /// The kernel refused the `--cwd` directory: it is not an absolute
    /// directory on the kernel's mount table.
    pub fn is_cwd_refusal(&self) -> bool {
        matches!(self, Self::Refused(message) if message.contains("kj context create: --cwd '"))
    }
}

/// A context created by [`ActorHandle::create_context_in_client_cwd`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedContext {
    pub id: ContextId,
    /// The kernel's refusal of the client's directory, when it refused it.
    /// The context was then created without `--cwd` and took the parent's
    /// cwd, if any. A client shows this to its user.
    pub cwd_refused: Option<String>,
}

impl std::fmt::Display for CreateContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call(error) => write!(f, "{error}"),
            Self::Refused(message) | Self::Unread(message) => f.write_str(message),
            Self::Held { refusal } => write!(f, "{refusal}"),
        }
    }
}

impl std::error::Error for CreateContextError {}

impl ActorHandle {
    /// Run `kj context create` from `parent`, authoring no blocks there, and
    /// return the new context's id. The caller joins it. A given `cwd` is
    /// stored before the create lifecycle runs, so rc sees it.
    pub async fn create_context_under(
        &self,
        parent: ContextId,
        label: &str,
        performer: &str,
        cwd: Option<&str>,
    ) -> Result<ContextId, CreateContextError> {
        let result = self
            .execute_kj_quiet(parent, context_create_argv(label, performer, cwd))
            .await
            .map_err(CreateContextError::Call)?;
        context_id_from_create_result(&result).map_err(CreateContextError::Refused)
    }

    /// The character that performs the contexts this session creates.
    ///
    /// `named` (`--character`) wins. Otherwise it is this connection's own
    /// character, which must not be a root: a person's context runs no
    /// model, so a person's connection with no `named` is refused with
    /// `how_to_name`, the client's own words for naming one.
    pub async fn performer_for_new_contexts(
        &self,
        parent: ContextId,
        named: Option<&str>,
        how_to_name: &str,
    ) -> Result<String, CreateContextError> {
        if let Some(named) = named {
            return Ok(named.to_string());
        }
        let me = self.whoami().await.map_err(CreateContextError::Call)?;
        let sheet = self
            .execute_kj_quiet(parent, vec!["character".into(), "show".into(), me.username.clone()])
            .await
            .map_err(CreateContextError::Call)?;
        let root = sheet
            .data
            .as_ref()
            .and_then(|data| data.get("root"))
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                CreateContextError::Unread(format!("character '{}' has no sheet: {}", me.username, sheet.stderr.trim()))
            })?;
        if root {
            return Err(CreateContextError::Refused(format!(
                "{} is a person (a root character), and a context a person performs runs no model. Name the \
                 character that performs this session's context with {how_to_name}, or connect with a non-root \
                 character's key",
                me.username
            )));
        }
        Ok(me.username)
    }

    /// Create from `parent` in `cwd`, the client's own working directory.
    ///
    /// The kernel may run on another host, where that directory does not
    /// exist. The create carries `--cwd-optional`, so the kernel decides when
    /// the create runs: it drops a directory it cannot use, the new context
    /// takes `parent`'s cwd, and the kernel's reason comes back in
    /// [`CreatedContext::cwd_refused`]. A create that a gate holds decides
    /// the same way when the reviewer allows it. A held create is
    /// [`CreateContextError::Held`]; every other refusal is an error. A
    /// client that must run in `cwd`, such as an ACP agent, calls
    /// [`Self::create_context_under`] instead.
    pub async fn create_context_in_client_cwd(
        &self,
        parent: ContextId,
        label: &str,
        performer: &str,
        cwd: Option<&str>,
    ) -> Result<CreatedContext, CreateContextError> {
        self.create_context_in_client_cwd_under(parent, label, performer, cwd, None).await
    }

    /// [`Self::create_context_in_client_cwd`] with `--parent <forked_from>`
    /// when one is given, as [`context_create_argv_under`] states it. A
    /// session under a root console names the root's MCP hub
    /// ([`Self::mcp_hub_of`]).
    pub async fn create_context_in_client_cwd_under(
        &self,
        parent: ContextId,
        label: &str,
        performer: &str,
        cwd: Option<&str>,
        forked_from: Option<&str>,
    ) -> Result<CreatedContext, CreateContextError> {
        let mut argv = context_create_argv_under(label, performer, cwd, forked_from);
        if cwd.is_some() {
            argv.push("--cwd-optional".to_string());
        }
        let result = match self.execute_kj_quiet(parent, argv).await {
            Ok(result) => result,
            Err(CallError::Refused(refusal)) if refusal.ask_id().is_some() => {
                return Err(CreateContextError::Held { refusal });
            }
            Err(error) => return Err(CreateContextError::Call(error)),
        };
        let id = context_id_from_create_result(&result).map_err(CreateContextError::Refused)?;
        let cwd_refused = result
            .data
            .as_ref()
            .and_then(|data| data.get("cwd_refused"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(CreatedContext { id, cwd_refused })
    }

    /// The MCP hub a session created from `parent` lands under, read from
    /// the person's sheet (`kj character show <person>`, its `mcp_hub_ctx`)
    /// when `parent` is a root console; `None` for any other parent. The
    /// rule is [`live_mcp_hub`].
    pub async fn mcp_hub_of(&self, parent: ContextId, contexts: &[ContextInfo]) -> Result<Option<ContextId>, CreateContextError> {
        let Some(info) = contexts.iter().find(|context| context.id == parent) else {
            return Err(CreateContextError::Refused(format!("no such context: {}", parent.short())));
        };
        if !is_root_context(info) {
            return Ok(None);
        }
        let Some(person) = info.performer.as_deref() else {
            return Err(CreateContextError::Unread(format!("root console '{}' has no performer", info.label)));
        };
        let sheet = self
            .execute_kj_quiet(parent, vec!["character".into(), "show".into(), person.to_string()])
            .await
            .map_err(CreateContextError::Call)?;
        let data = sheet.data.as_ref().ok_or_else(|| {
            CreateContextError::Unread(format!("character '{person}' has no sheet: {}", sheet.stderr.trim()))
        })?;
        let hub = match data.get("mcp_hub_ctx").and_then(serde_json::Value::as_str) {
            None => None,
            Some(hex) => Some(ContextId::parse(hex).map_err(|e| {
                CreateContextError::Unread(format!("character '{person}' names an invalid MCP hub '{hex}': {e}"))
            })?),
        };
        live_mcp_hub(info, hub, contexts).map_err(CreateContextError::Refused)
    }

    /// Archive `context_id` with `kj context archive <id> --confirm`, run
    /// from [`archive_runs_from`] and authoring no blocks there. Its archive
    /// rc lifecycle runs. A refusal is an error carrying the kernel's
    /// message.
    pub async fn archive_context_from_parent(&self, context_id: ContextId) -> Result<(), String> {
        let contexts = self.list_contexts().await.map_err(|e| format!("could not list contexts: {e}"))?;
        let from = archive_runs_from(context_id, &contexts)?;
        let result = self
            .execute_kj_quiet(from, context_archive_argv(context_id))
            .await
            .map_err(|e| e.to_string())?;
        if result.exit_code != 0 {
            let detail = if result.stderr.trim().is_empty() { &result.stdout } else { &result.stderr };
            return Err(detail.trim().to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_runs_from_the_parent_and_confirms() {
        let root = root("amy");
        let seat = ctx("seat", Some(root.id));
        let contexts = vec![root.clone(), seat.clone()];
        assert_eq!(archive_runs_from(seat.id, &contexts), Ok(root.id));
        assert_eq!(archive_runs_from(root.id, &contexts), Ok(root.id), "a root runs its own archive");
        assert!(archive_runs_from(ContextId::new(), &contexts).is_err());
        assert_eq!(context_archive_argv(seat.id), vec!["context", "archive", &seat.id.to_hex(), "--confirm"]);
    }

    /// A person's root console, by the wire facts.
    fn root(label: &str) -> ContextInfo {
        let mut info = ctx(label, None);
        info.performer = Some(label.to_string());
        info.performed_by_person = true;
        info.is_root_console = true;
        info
    }

    /// A context a model character performs, or none when `label` is a hub.
    fn ctx(label: &str, parent: Option<ContextId>) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from: parent,
            provider: String::new(),
            model: String::new(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: kaijutsu_types::Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
            input_tokens: None,
            cost: None,
            performer: Some("coder".to_string()),
            performed_by_person: false,
            is_root_console: false,
            is_console: false,
            is_mcp_session: false,
        }
    }

    #[test]
    fn the_only_live_root_is_chosen_when_nothing_is_named() {
        let amy = root("amy");
        let banto = ctx("banto", Some(amy.id));
        let mut old = root("old");
        old.archived = true;
        let choice = choose_parent(None, &[banto, old, amy.clone()]).unwrap();
        assert_eq!(choice, ParentChoice { context_id: amy.id, label: "amy".into(), source: ParentSource::OnlyRoot });
    }

    #[test]
    fn a_named_parent_wins_by_label_or_id() {
        let amy = root("amy");
        let bob = root("bob");
        let banto = ctx("banto", Some(amy.id));
        let contexts = [amy.clone(), bob.clone(), banto.clone()];

        let by_label = choose_parent(Some("banto"), &contexts).unwrap();
        assert_eq!((by_label.context_id, by_label.source), (banto.id, ParentSource::Explicit));
        let by_id = choose_parent(Some(&bob.id.to_hex()), &contexts).unwrap();
        assert_eq!(by_id.context_id, bob.id);
    }

    #[test]
    fn a_named_parent_must_be_live() {
        let mut gone = ctx("gone", None);
        gone.concluded_at = Some(5);
        let error = choose_parent(Some("gone"), &[gone]).unwrap_err();
        assert!(error.contains("gone"), "{error}");
    }

    #[test]
    fn no_root_or_several_roots_refuse_and_say_why() {
        let none = choose_parent(None, &[ctx("lane", None)]).unwrap_err();
        assert!(none.contains("kaijutsu-server init"), "{none}");

        let several = choose_parent(None, &[root("amy"), root("bob")]).unwrap_err();
        assert!(several.contains("amy") && several.contains("bob") && several.contains("--parent"), "{several}");
    }

    /// Only the wire fact makes a root console: a parentless context with
    /// no performer is not one, and neither is a person's other context.
    ///
    /// Falsified by deciding from a missing parent or from the performer.
    #[test]
    fn a_parentless_context_that_is_no_root_console_is_not_a_root() {
        let mut person_child = ctx("amy-shell", None);
        person_child.performed_by_person = true;
        for context in [ctx("handoff/banto", None), person_child] {
            let error = choose_parent(None, &[context]).unwrap_err();
            assert!(error.contains("no live root context"), "{error}");
        }
    }

    /// The argv names the performer and no type: the performer's rc
    /// directory is what the context runs.
    ///
    /// Falsified by an argv that carries `--type`.
    #[test]
    fn argv_always_names_the_performer_and_names_the_cwd_only_when_given() {
        assert_eq!(context_create_argv("lane", "coder", None), ["context", "create", "coder", "--name", "lane"]);
        assert_eq!(
            context_create_argv("lane", "coder", Some("/app")),
            ["context", "create", "coder", "--name", "lane", "--cwd", "/app"]
        );
    }

    /// A session created from a root console lands under the hub its
    /// person's sheet names; from any other context it lands under no hub,
    /// whatever the sheet says.
    ///
    /// Falsified by returning the hub for a parent that is not a root
    /// console, or by finding the hub by its label.
    #[test]
    fn a_root_console_parent_names_its_mcp_hub() {
        let amy = root("amy");
        let mut hub = ctx("not-the-label-mcp/amy", None);
        hub.performer = None;
        let banto = ctx("banto", Some(amy.id));
        let labeled = ctx("mcp/amy", None);
        let contexts = [amy.clone(), hub.clone(), banto.clone(), labeled];
        assert_eq!(live_mcp_hub(&amy, Some(hub.id), &contexts), Ok(Some(hub.id)));
        assert_eq!(live_mcp_hub(&banto, Some(hub.id), &contexts), Ok(None), "a named non-root parent behaves as before");
    }

    #[test]
    fn a_root_console_with_no_live_hub_refuses_and_says_why() {
        let amy = root("amy");
        let mut archived = ctx("mcp/amy", None);
        archived.archived = true;
        let gone = ContextId::new();
        let labeled = ctx("mcp/amy", None);
        for (hub, contexts) in [
            (None, vec![amy.clone(), labeled]),
            (Some(archived.id), vec![amy.clone(), archived.clone()]),
            (Some(gone), vec![amy.clone()]),
        ] {
            let error = live_mcp_hub(&amy, hub, &contexts).unwrap_err();
            assert!(error.contains("MCP hub") && error.contains("amy"), "{error}");
        }
    }

    #[test]
    fn argv_names_the_forked_from_context_only_when_given() {
        assert_eq!(
            context_create_argv_under("s", "claude", Some("/app"), Some("mcp/amy")),
            ["context", "create", "claude", "--name", "s", "--cwd", "/app", "--parent", "mcp/amy"]
        );
        assert_eq!(context_create_argv_under("s", "claude", None, None), context_create_argv("s", "claude", None));
    }

    /// An MCP session runs its own archive, by the wire fact; its hub may
    /// hold no loadout.
    ///
    /// Falsified by deciding from the parent's label or the session's.
    #[test]
    fn an_mcp_session_archives_itself() {
        let hub = ctx("mcp/amy", None);
        let mut session = ctx("session", Some(hub.id));
        session.is_mcp_session = true;
        let lane = ctx("lane", Some(hub.id));
        let contexts = vec![hub.clone(), session.clone(), lane.clone()];
        assert_eq!(archive_runs_from(session.id, &contexts), Ok(session.id));
        assert_eq!(archive_runs_from(lane.id, &contexts), Ok(hub.id), "a context under the hub that is no session");
    }

    fn result(exit_code: i32, stderr: &str, data: Option<serde_json::Value>) -> KjExecutionResult {
        KjExecutionResult {
            exit_code,
            stdout: String::new(),
            stderr: stderr.to_string(),
            command_block_id: None,
            latch: None,
            data,
        }
    }

    #[test]
    fn only_a_kernel_refusal_can_be_a_label_conflict() {
        assert!(CreateContextError::Refused("kj context create: label conflict: lane".into()).is_label_conflict());
        assert!(!CreateContextError::Refused("kj context create: unknown context type".into()).is_label_conflict());
        assert!(!CreateContextError::Call(CallError::PermanentlyFailed("label conflict".into())).is_label_conflict());
        assert!(!CreateContextError::Call(CallError::Shutdown).is_label_conflict());
    }

    #[test]
    fn only_a_kernel_refusal_of_the_directory_is_a_cwd_refusal() {
        for refusal in [
            "kj context create: --cwd '/gone' is unavailable: not found",
            "kj context create: --cwd '/etc/hosts' is not a directory",
            "kj context create: --cwd 'rel' is not an absolute path",
        ] {
            assert!(CreateContextError::Refused(refusal.into()).is_cwd_refusal(), "{refusal}");
        }
        assert!(!CreateContextError::Refused("kj context create: label conflict: lane".into()).is_cwd_refusal());
        assert!(!CreateContextError::Call(CallError::PermanentlyFailed("--cwd '/x'".into())).is_cwd_refusal());
    }

    #[test]
    fn the_create_result_yields_the_id_or_the_kernel_message() {
        let id = ContextId::new();
        let ok = result(0, "", Some(serde_json::json!({ "context_id": id.to_hex() })));
        assert_eq!(context_id_from_create_result(&ok), Ok(id));

        let refused = result(1, "kj context create: label conflict\n", None);
        assert_eq!(context_id_from_create_result(&refused), Err("kj context create: label conflict".to_string()));

        let missing = result(0, "", None);
        assert!(context_id_from_create_result(&missing).unwrap_err().contains("without context_id"));
    }
}
