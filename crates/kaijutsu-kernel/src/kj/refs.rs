//! Context reference parsing for kj commands.
//!
//! Supports `.` (current), `.parent` / `.senpai` chains, labels, and hex prefixes.

use kaijutsu_types::ContextId;

use crate::kernel_db::KernelDb;

use super::KjCaller;

/// A parsed context reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRef {
    /// `.` — the caller's current context.
    Current,
    /// `.parent`, `.senpai`, `.parent.senpai`, etc. — one step per segment,
    /// in any order, from the current context.
    Chain(Vec<RefStep>),
    /// A label or hex prefix — resolved via KernelDb.
    Query(String),
}

/// One segment of a [`ContextRef::Chain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefStep {
    /// `.parent` — the context this one was forked from (`forked_from`).
    Parent,
    /// `.senpai` — the context this one answers to (`senpai_ctx`).
    Senpai,
}

impl RefStep {
    fn word(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Senpai => "senpai",
        }
    }
}

/// Parse a context reference string into a ContextRef.
pub fn parse_context_ref(s: &str) -> ContextRef {
    if s == "." {
        return ContextRef::Current;
    }

    // Exactly a chain of `.parent` and `.senpai` segments; anything else is a query.
    if let Some(rest) = s.strip_prefix('.') {
        let steps: Option<Vec<RefStep>> = rest
            .split('.')
            .map(|segment| match segment {
                "parent" => Some(RefStep::Parent),
                "senpai" => Some(RefStep::Senpai),
                _ => None,
            })
            .collect();
        if let Some(steps) = steps {
            return ContextRef::Chain(steps);
        }
    }

    ContextRef::Query(s.to_string())
}

/// Resolve a context reference to a ContextId.
pub fn resolve_context_ref(
    ctx_ref: &ContextRef,
    caller: &KjCaller,
    db: &KernelDb,
) -> Result<ContextId, String> {
    resolve_context_ref_from(ctx_ref, caller.context_id, db)
}

/// Resolve a context reference against `current`, the context `.` names.
pub fn resolve_context_ref_from(
    ctx_ref: &ContextRef,
    current: Option<ContextId>,
    db: &KernelDb,
) -> Result<ContextId, String> {
    match ctx_ref {
        ContextRef::Current => current.ok_or_else(|| "no active context joined".to_string()),
        ContextRef::Chain(steps) => {
            let mut current = current.ok_or_else(|| "no active context joined".to_string())?;
            let written = || steps.iter().map(|step| format!(".{}", step.word())).collect::<String>();
            for (i, step) in steps.iter().enumerate() {
                let row = db
                    .get_context(current)
                    .map_err(|e| format!("db error: {e}"))?
                    .ok_or_else(|| {
                        format!("context {} not found at step {} of {}", current.short(), i + 1, written())
                    })?;
                let next = match step {
                    RefStep::Parent => row.forked_from,
                    RefStep::Senpai => row.senpai_ctx,
                };
                current = next.ok_or_else(|| {
                    let short = current.short();
                    let label = row.label.as_deref().unwrap_or(&short);
                    format!("context '{label}' has no {} (step {} of {})", step.word(), i + 1, written())
                })?;
            }
            Ok(current)
        }
        ContextRef::Query(query) => db.resolve_context(query).map_err(|e| e.to_string()),
    }
}

/// Resolve a context reference string, defaulting to `.` (current) if empty/None.
pub fn resolve_context_arg(
    arg: Option<&str>,
    caller: &KjCaller,
    db: &KernelDb,
) -> Result<ContextId, String> {
    resolve_context_arg_from(arg, caller.context_id, db)
}

/// [`resolve_context_arg`] against `current`, the context `.` names.
pub fn resolve_context_arg_from(
    arg: Option<&str>,
    current: Option<ContextId>,
    db: &KernelDb,
) -> Result<ContextId, String> {
    let ctx_ref = match arg {
        Some(s) if !s.is_empty() => parse_context_ref(s),
        _ => ContextRef::Current,
    };
    resolve_context_ref_from(&ctx_ref, current, db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_current() {
        assert_eq!(parse_context_ref("."), ContextRef::Current);
    }

    #[test]
    fn parse_parent() {
        assert_eq!(parse_context_ref(".parent"), ContextRef::Chain(vec![RefStep::Parent]));
    }

    #[test]
    fn parse_grandparent() {
        assert_eq!(
            parse_context_ref(".parent.parent"),
            ContextRef::Chain(vec![RefStep::Parent, RefStep::Parent])
        );
    }

    #[test]
    fn parse_senpai_and_mixed_chains() {
        use RefStep::*;
        assert_eq!(parse_context_ref(".senpai"), ContextRef::Chain(vec![Senpai]));
        assert_eq!(parse_context_ref(".senpai.senpai"), ContextRef::Chain(vec![Senpai, Senpai]));
        assert_eq!(parse_context_ref(".parent.senpai"), ContextRef::Chain(vec![Parent, Senpai]));
        assert_eq!(parse_context_ref(".senpai.parent.senpai"), ContextRef::Chain(vec![Senpai, Parent, Senpai]));
    }

    #[test]
    fn parse_not_exactly_a_chain_is_a_query() {
        for s in [".senpaifoo", ".senpai.", ".senpai..parent", ".parent.foo", "senpai", ".senpai.parentx", "..parent"] {
            assert_eq!(parse_context_ref(s), ContextRef::Query(s.to_string()), "{s}");
        }
    }

    #[test]
    fn parse_label() {
        assert_eq!(
            parse_context_ref("default"),
            ContextRef::Query("default".to_string())
        );
    }

    #[test]
    fn parse_hex() {
        assert_eq!(
            parse_context_ref("019c779b"),
            ContextRef::Query("019c779b".to_string())
        );
    }

    #[test]
    fn parse_dotparent_malformed_is_query() {
        // ".parentfoo" doesn't match the exact pattern
        assert_eq!(
            parse_context_ref(".parentfoo"),
            ContextRef::Query(".parentfoo".to_string())
        );
    }

    #[test]
    fn resolve_current() {
        let db = KernelDb::temporary().unwrap();
        let ctx_id = ContextId::new();
        let caller = KjCaller {
            principal_id: kaijutsu_types::PrincipalId::new(),
            actor_id: kaijutsu_types::PrincipalId::new(),
            reviewer_id: None,
            context_id: Some(ctx_id),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let result = resolve_context_ref(&ContextRef::Current, &caller, &db);
        assert_eq!(result.unwrap(), ctx_id);
    }

    #[test]
    fn resolve_parent_no_parent() {
        let db = KernelDb::temporary().unwrap();
        let ctx_id = ContextId::new();
        let principal = kaijutsu_types::PrincipalId::new();

        // Insert context with no parent
        let row = crate::kernel_db::ContextRow {
            context_id: ctx_id,
            label: Some("root".to_string()),
            system_prompt: None,
            context_state: kaijutsu_types::ContextState::Live,
            context_type: "default".to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: principal,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            origin_host: None,
            performer: None, senpai_ctx: None, persona: None,
        };
        let ws_id = db.get_or_create_default_workspace(principal).unwrap();
        db.insert_context_with_document(&row, ws_id).unwrap();

        let caller = KjCaller {
            principal_id: principal,
            actor_id: principal,
            reviewer_id: None,
            context_id: Some(ctx_id),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let result = resolve_context_ref(&ContextRef::Chain(vec![RefStep::Parent]), &caller, &db);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no parent"));
    }

    #[test]
    fn resolve_parent_chain() {
        let db = KernelDb::temporary().unwrap();
        let principal = kaijutsu_types::PrincipalId::new();

        let grandparent_id = ContextId::new();
        let parent_id = ContextId::new();
        let child_id = ContextId::new();
        let ws_id = db.get_or_create_default_workspace(principal).unwrap();

        // Insert grandparent
        db.insert_context_with_document(
            &crate::kernel_db::ContextRow {
                context_id: grandparent_id,
                label: Some("grandparent".to_string()),
                system_prompt: None,
                context_state: kaijutsu_types::ContextState::Live,
                context_type: "default".to_string(),
                created_at: 1000,
                created_by: principal,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                origin_host: None,
                performer: None, senpai_ctx: None, persona: None,
            },
            ws_id,
        )
        .unwrap();

        // Insert parent
        db.insert_context_with_document(
            &crate::kernel_db::ContextRow {
                context_id: parent_id,
                label: Some("parent".to_string()),
                system_prompt: None,
                context_state: kaijutsu_types::ContextState::Live,
                context_type: "default".to_string(),
                created_at: 2000,
                created_by: principal,
                forked_from: Some(grandparent_id),
                fork_kind: Some(kaijutsu_types::ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                origin_host: None,
                performer: None, senpai_ctx: None, persona: None,
            },
            ws_id,
        )
        .unwrap();

        // Insert child
        db.insert_context_with_document(
            &crate::kernel_db::ContextRow {
                context_id: child_id,
                label: Some("child".to_string()),
                system_prompt: None,
                context_state: kaijutsu_types::ContextState::Live,
                context_type: "default".to_string(),
                created_at: 3000,
                created_by: principal,
                forked_from: Some(parent_id),
                fork_kind: Some(kaijutsu_types::ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                origin_host: None,
                performer: None, senpai_ctx: None, persona: None,
            },
            ws_id,
        )
        .unwrap();

        let caller = KjCaller {
            principal_id: principal,
            actor_id: principal,
            reviewer_id: None,
            context_id: Some(child_id),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        // .parent → parent_id
        let result = resolve_context_ref(&ContextRef::Chain(vec![RefStep::Parent]), &caller, &db);
        assert_eq!(result.unwrap(), parent_id);

        // .parent.parent → grandparent_id
        let result = resolve_context_ref(&ContextRef::Chain(vec![RefStep::Parent; 2]), &caller, &db);
        assert_eq!(result.unwrap(), grandparent_id);

        // .parent.parent.parent → error (grandparent has no parent)
        let result = resolve_context_ref(&ContextRef::Chain(vec![RefStep::Parent; 3]), &caller, &db);
        assert!(result.is_err());
    }
}
