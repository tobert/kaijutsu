//! Capture durable exports referenced by a shell plan.
//!
//! Statement and non-literal heredoc free variables form one deduplicated set.
//! The approval description and hook plan use this reader so they apply the same
//! rule. A missing export is recorded as unset; a storage fault returns an error.
//! Command substitution and interpreter-provided defaults are not durable exports.

use std::collections::{HashMap, HashSet};

use kaijutsu_types::ContextId;

use crate::kernel_db::KernelDb;

/// One free variable's value at ask time. `value` is `None` when the
/// variable was unset in `context_env` — a row is recorded for every free
/// variable name regardless, so an unset variable and no snapshot at all
/// are never confused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskEnvEntry {
    pub name: String,
    pub value: Option<String>,
}

/// The union of every statement's free variables — statement-level and
/// heredoc-level — deduplicated in first-seen order. The module's one
/// definition of "free"; see the module docs.
fn free_variable_names(statements: &[kaish_kernel::PlannedStatement]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for ps in statements {
        for name in &ps.plan.free_variables {
            if seen.insert(name.clone()) {
                names.push(name.clone());
            }
        }
        for cmd in &ps.plan.commands {
            for heredoc in &cmd.heredocs {
                for name in &heredoc.free_variables {
                    if seen.insert(name.clone()) {
                        names.push(name.clone());
                    }
                }
            }
        }
    }
    names
}

/// The value every free variable in `statements` held at ask time, read
/// from `context_id`'s durable `context_env` — the same source a
/// materialized shell reads when applying durable exports.
///
/// Called from exactly two places: `kj::gate::build_ask`, so the ask
/// records what a human is approving, and the broker's `KJ_TOOL_PLAN`
/// builder, so the lfm2d classifier hook sees the same data. Both must
/// call this rather than deriving their own free-variable set or reading
/// `context_env` directly.
///
/// Read failures must reach the caller. An unreadable value cannot be
/// recorded as unset or presented to a reviewer as a captured input.
pub fn free_variable_values(
    db: &KernelDb,
    context_id: ContextId,
    statements: &[kaish_kernel::PlannedStatement],
) -> Result<Vec<AskEnvEntry>, String> {
    let names = free_variable_names(statements);
    if names.is_empty() {
        return Ok(Vec::new());
    }

    let known: HashMap<String, String> = db.get_context_env(context_id)
        .map_err(|error| format!("could not read context_env for {context_id}: {error}"))?
        .into_iter().map(|row| (row.key, row.value)).collect();

    Ok(names
        .into_iter()
        .map(|name| {
            let value = known.get(&name).cloned();
            AskEnvEntry { name, value }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{register_context, test_dispatcher_with_timeouts};
    use kaijutsu_types::{PrincipalId, TimeoutPolicy};

    fn planned(source: &str) -> Vec<kaish_kernel::PlannedStatement> {
        kaish_kernel::plan_program(source).expect("test source must parse")
    }

    /// No free variables anywhere in the submission — the common case for
    /// a `shell_write` statement with nothing to snapshot.
    #[tokio::test]
    async fn no_free_variables_snapshots_nothing() {
        let d = test_dispatcher_with_timeouts(TimeoutPolicy::default()).await;
        let ctx_id = register_context(&d, Some("env-empty"), None, PrincipalId::new());
        let entries = free_variable_values(&d.kernel_db.lock(), ctx_id, &planned("echo hi")).unwrap();
        assert!(entries.is_empty());
    }

    /// A name present in `context_env` comes back with its value; a name
    /// absent comes back `None` — never dropped, never a default string.
    ///
    /// Falsified by returning early on the first `context_env` miss instead
    /// of recording `None` and continuing: `BAZ` would be silently absent
    /// from `entries` rather than present and unset.
    #[tokio::test]
    async fn a_free_variable_reads_its_context_env_value_and_an_unset_one_reads_none() {
        let d = test_dispatcher_with_timeouts(TimeoutPolicy::default()).await;
        let ctx_id = register_context(&d, Some("env-mixed"), None, PrincipalId::new());
        d.kernel_db.lock().set_context_env(ctx_id, "FOO", "bar").unwrap();

        let entries = free_variable_values(&d.kernel_db.lock(), ctx_id, &planned("echo ${FOO} ${BAZ}")).unwrap();
        let mut by_name: HashMap<String, Option<String>> =
            entries.into_iter().map(|e| (e.name, e.value)).collect();
        assert_eq!(by_name.remove("FOO"), Some(Some("bar".to_string())));
        assert_eq!(by_name.remove("BAZ"), Some(None));
        assert!(by_name.is_empty(), "no other names should appear");
    }

    /// The union covers a non-literal heredoc's free variables too, not
    /// just the statement's own argv-level reads — this is the half
    /// `GatedStatement::vars` cannot see (`kj::shell_gate`'s module docs).
    ///
    /// Falsified by only walking `ps.plan.free_variables` and skipping the
    /// heredoc loop: `LOG` would never appear.
    #[tokio::test]
    async fn the_union_includes_heredoc_free_variables() {
        let d = test_dispatcher_with_timeouts(TimeoutPolicy::default()).await;
        let ctx_id = register_context(&d, Some("env-heredoc"), None, PrincipalId::new());
        d.kernel_db.lock().set_context_env(ctx_id, "LOG", "value").unwrap();

        let source = "cat <<EOF\n${LOG}\nEOF\n";
        let entries = free_variable_values(&d.kernel_db.lock(), ctx_id, &planned(source)).unwrap();
        assert!(
            entries.iter().any(|e| e.name == "LOG" && e.value.as_deref() == Some("value")),
            "the heredoc's free variable must appear in the snapshot: {entries:?}"
        );
    }

    /// Duplicate names across statements or between a statement and its own
    /// heredoc collapse to one entry, first-seen.
    #[tokio::test]
    async fn duplicate_free_variable_names_appear_once() {
        let d = test_dispatcher_with_timeouts(TimeoutPolicy::default()).await;
        let ctx_id = register_context(&d, Some("env-dedup"), None, PrincipalId::new());
        d.kernel_db.lock().set_context_env(ctx_id, "FOO", "bar").unwrap();

        let source = "echo ${FOO}\necho ${FOO}\n";
        let entries = free_variable_values(&d.kernel_db.lock(), ctx_id, &planned(source)).unwrap();
        assert_eq!(entries.iter().filter(|e| e.name == "FOO").count(), 1);
    }
}
