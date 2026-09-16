//! Create a context through `kj context create`.
//!
//! The kernel has no RPC for creating a context. A client runs `kj context
//! create` from an existing context, which becomes the new context's parent,
//! so every context has a root above it (`docs/character.md`, "Bootstrap:
//! the person creates themself"). The kernel never chooses that parent; a
//! client that has no context yet chooses one with [`choose_parent`].

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
    context.context_type == "root" && context.forked_from.is_none()
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

/// The `kj` argv that creates `label` of `context_type`, played by
/// `performer` when one is given.
pub fn context_create_argv(label: &str, context_type: &str, performer: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "context".to_string(),
        "create".to_string(),
        label.to_string(),
        "--type".to_string(),
        context_type.to_string(),
    ];
    if let Some(performer) = performer {
        argv.push("--as".to_string());
        argv.push(performer.to_string());
    }
    argv
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
}

impl CreateContextError {
    /// The kernel refused because another live context holds the label.
    pub fn is_label_conflict(&self) -> bool {
        matches!(self, Self::Refused(message) if message.contains("label conflict"))
    }
}

impl std::fmt::Display for CreateContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call(error) => write!(f, "{error}"),
            Self::Refused(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CreateContextError {}

impl ActorHandle {
    /// Run `kj context create` from `parent`, authoring no blocks there, and
    /// return the new context's id. The caller joins it.
    pub async fn create_context_under(
        &self,
        parent: ContextId,
        label: &str,
        context_type: &str,
        performer: Option<&str>,
    ) -> Result<ContextId, CreateContextError> {
        let result = self
            .execute_kj_quiet(parent, context_create_argv(label, context_type, performer))
            .await
            .map_err(CreateContextError::Call)?;
        context_id_from_create_result(&result).map_err(CreateContextError::Refused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(label: &str, context_type: &str, parent: Option<ContextId>) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from: parent,
            provider: String::new(),
            model: String::new(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: context_type.to_string(),
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
            cast_label: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
        }
    }

    #[test]
    fn the_only_live_root_is_chosen_when_nothing_is_named() {
        let amy = ctx("amy", "root", None);
        let banto = ctx("banto", "director", Some(amy.id));
        let mut old = ctx("old", "root", None);
        old.archived = true;
        let choice = choose_parent(None, &[banto, old, amy.clone()]).unwrap();
        assert_eq!(choice, ParentChoice { context_id: amy.id, label: "amy".into(), source: ParentSource::OnlyRoot });
    }

    #[test]
    fn a_named_parent_wins_by_label_or_id() {
        let amy = ctx("amy", "root", None);
        let bob = ctx("bob", "root", None);
        let banto = ctx("banto", "director", Some(amy.id));
        let contexts = [amy.clone(), bob.clone(), banto.clone()];

        let by_label = choose_parent(Some("banto"), &contexts).unwrap();
        assert_eq!((by_label.context_id, by_label.source), (banto.id, ParentSource::Explicit));
        let by_id = choose_parent(Some(&bob.id.to_hex()), &contexts).unwrap();
        assert_eq!(by_id.context_id, bob.id);
    }

    #[test]
    fn a_named_parent_must_be_live() {
        let mut gone = ctx("gone", "coder", None);
        gone.concluded_at = Some(5);
        let error = choose_parent(Some("gone"), &[gone]).unwrap_err();
        assert!(error.contains("gone"), "{error}");
    }

    #[test]
    fn no_root_or_several_roots_refuse_and_say_why() {
        let none = choose_parent(None, &[ctx("lane", "coder", None)]).unwrap_err();
        assert!(none.contains("kaijutsu-server init"), "{none}");

        let several = choose_parent(None, &[ctx("amy", "root", None), ctx("bob", "root", None)]).unwrap_err();
        assert!(several.contains("amy") && several.contains("bob") && several.contains("--parent"), "{several}");
    }

    #[test]
    fn a_parentless_non_root_type_is_not_a_root() {
        let error = choose_parent(None, &[ctx("handoff/banto", "handoff", None)]).unwrap_err();
        assert!(error.contains("no live root context"), "{error}");
    }

    #[test]
    fn argv_names_the_performer_only_when_given() {
        assert_eq!(context_create_argv("lane", "coder", None), ["context", "create", "lane", "--type", "coder"]);
        assert_eq!(
            context_create_argv("lane", "coder", Some("coder")),
            ["context", "create", "lane", "--type", "coder", "--as", "coder"]
        );
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
