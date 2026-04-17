//! Context reference parsing for kj commands.
//!
//! Supports `.` (current), `.parent` chains, labels, and hex prefixes.

use kaijutsu_types::ContextId;

use crate::kernel_db::KernelDb;

use super::KjCaller;

/// A parsed context reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRef {
    /// `.` — the caller's current context.
    Current,
    /// `.parent`, `.parent.parent`, etc. — walk up fork lineage.
    Parent(usize),
    /// A label or hex prefix — resolved via KernelDb.
    Query(String),
}

/// Parse a context reference string into a ContextRef.
pub fn parse_context_ref(s: &str) -> ContextRef {
    if s == "." {
        return ContextRef::Current;
    }

    if s.starts_with(".parent") {
        // Count the chain: .parent = 1, .parent.parent = 2, etc.
        let depth = s.matches(".parent").count();
        // Validate the format: should be exactly ".parent" repeated
        let expected = (0..depth).map(|_| ".parent").collect::<String>();
        if s == expected {
            return ContextRef::Parent(depth);
        }
        // If it doesn't match the exact pattern, treat as a query
    }

    ContextRef::Query(s.to_string())
}

/// Resolve a context reference to a ContextId.
pub fn resolve_context_ref(
    ctx_ref: &ContextRef,
    caller: &KjCaller,
    db: &KernelDb,
    kernel_id: kaijutsu_types::KernelId,
) -> Result<ContextId, String> {
    match ctx_ref {
        ContextRef::Current => caller
            .context_id
            .ok_or_else(|| "no active context joined".to_string()),
        ContextRef::Parent(depth) => {
            let mut current = caller
                .context_id
                .ok_or_else(|| "no active context joined".to_string())?;
            for i in 0..*depth {
                let row = db
                    .get_context(current)
                    .map_err(|e| format!("db error: {e}"))?
                    .ok_or_else(|| {
                        format!(
                            "context {} not found at parent depth {}",
                            current.short(),
                            i
                        )
                    })?;
                current = row.forked_from.ok_or_else(|| {
                    let short = current.short();
                    let label = row.label.as_deref().unwrap_or(&short);
                    format!("context '{}' has no parent (at depth {})", label, i + 1)
                })?;
            }
            Ok(current)
        }
        ContextRef::Query(query) => db
            .resolve_context(kernel_id, query)
            .map_err(|e| e.to_string()),
    }
}

/// Resolve a context reference string, defaulting to `.` (current) if empty/None.
pub fn resolve_context_arg(
    arg: Option<&str>,
    caller: &KjCaller,
    db: &KernelDb,
    kernel_id: kaijutsu_types::KernelId,
) -> Result<ContextId, String> {
    let ctx_ref = match arg {
        Some(s) if !s.is_empty() => parse_context_ref(s),
        _ => ContextRef::Current,
    };
    resolve_context_ref(&ctx_ref, caller, db, kernel_id)
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
        assert_eq!(parse_context_ref(".parent"), ContextRef::Parent(1));
    }

    #[test]
    fn parse_grandparent() {
        assert_eq!(parse_context_ref(".parent.parent"), ContextRef::Parent(2));
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
        let db = KernelDb::in_memory().unwrap();
        let kid = kaijutsu_types::KernelId::new();
        let ctx_id = ContextId::new();
        let caller = KjCaller {
            principal_id: kaijutsu_types::PrincipalId::new(),
            context_id: Some(ctx_id),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
        };

        let result = resolve_context_ref(&ContextRef::Current, &caller, &db, kid);
        assert_eq!(result.unwrap(), ctx_id);
    }

    #[test]
    fn resolve_parent_no_parent() {
        let db = KernelDb::in_memory().unwrap();
        let kid = kaijutsu_types::KernelId::new();
        let ctx_id = ContextId::new();
        let principal = kaijutsu_types::PrincipalId::new();

        // Insert context with no parent
        let row = crate::kernel_db::ContextRow {
            context_id: ctx_id,
            kernel_id: kid,
            label: Some("root".to_string()),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: kaijutsu_types::ConsentMode::Collaborative,
            context_state: kaijutsu_types::ContextState::Live,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: principal,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        };
        let ws_id = db.get_or_create_default_workspace(kid, principal).unwrap();
        db.insert_context_with_document(&row, ws_id).unwrap();

        let caller = KjCaller {
            principal_id: principal,
            context_id: Some(ctx_id),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
        };

        let result = resolve_context_ref(&ContextRef::Parent(1), &caller, &db, kid);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no parent"));
    }

    #[test]
    fn resolve_parent_chain() {
        let db = KernelDb::in_memory().unwrap();
        let kid = kaijutsu_types::KernelId::new();
        let principal = kaijutsu_types::PrincipalId::new();

        let grandparent_id = ContextId::new();
        let parent_id = ContextId::new();
        let child_id = ContextId::new();
        let ws_id = db.get_or_create_default_workspace(kid, principal).unwrap();

        // Insert grandparent
        db.insert_context_with_document(
            &crate::kernel_db::ContextRow {
                context_id: grandparent_id,
                kernel_id: kid,
                label: Some("grandparent".to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                context_state: kaijutsu_types::ContextState::Live,
                created_at: 1000,
                created_by: principal,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            },
            ws_id,
        )
        .unwrap();

        // Insert parent
        db.insert_context_with_document(
            &crate::kernel_db::ContextRow {
                context_id: parent_id,
                kernel_id: kid,
                label: Some("parent".to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                context_state: kaijutsu_types::ContextState::Live,
                created_at: 2000,
                created_by: principal,
                forked_from: Some(grandparent_id),
                fork_kind: Some(kaijutsu_types::ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            },
            ws_id,
        )
        .unwrap();

        // Insert child
        db.insert_context_with_document(
            &crate::kernel_db::ContextRow {
                context_id: child_id,
                kernel_id: kid,
                label: Some("child".to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                context_state: kaijutsu_types::ContextState::Live,
                created_at: 3000,
                created_by: principal,
                forked_from: Some(parent_id),
                fork_kind: Some(kaijutsu_types::ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            },
            ws_id,
        )
        .unwrap();

        let caller = KjCaller {
            principal_id: principal,
            context_id: Some(child_id),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
        };

        // .parent → parent_id
        let result = resolve_context_ref(&ContextRef::Parent(1), &caller, &db, kid);
        assert_eq!(result.unwrap(), parent_id);

        // .parent.parent → grandparent_id
        let result = resolve_context_ref(&ContextRef::Parent(2), &caller, &db, kid);
        assert_eq!(result.unwrap(), grandparent_id);

        // .parent.parent.parent → error (grandparent has no parent)
        let result = resolve_context_ref(&ContextRef::Parent(3), &caller, &db, kid);
        assert!(result.is_err());
    }
}
