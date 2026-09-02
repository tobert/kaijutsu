//! The free-variable value snapshot an ask records at ask time — computed
//! once here so the human reviewing the ask and the lfm2d classifier
//! reading `KJ_TOOL_PLAN` agree on what "free" means and what each free
//! variable held.
//!
//! ## The union rule
//!
//! A statement's free variables are `Plan::free_variables` — everything the
//! statement reads without lexically binding. A non-literal heredoc adds
//! more: `PlannedHeredoc::free_variables` names the variables THAT body
//! substitutes, which `Plan::free_variables` does not repeat (heredoc
//! expansion is a property of the redirect, not of the command's own
//! argv — `kj::shell_gate`'s module docs). The snapshot is the union of
//! both sets, over every statement in the submission, deduplicated in
//! first-seen order. This function is the one place that union is taken;
//! a reader must not re-derive "free" from `GatedStatement::vars`, which
//! only carries the statement-level half.
//!
//! ## What a value comes from, and what it deliberately does not cover
//!
//! Both gated shell paths run on a single-use materialized shell seeded
//! ONLY from durable state — `context_env` rows plus the cwd, with no
//! transient scope surviving between submissions (`docs/gate-shape-b.md`).
//! So a free variable's value at ask time is exactly its `context_env`
//! value, or `None` when it is unset there — not an estimate of what kaish
//! would substitute, the literal source the substitution reads.
//!
//! What it cannot cover: a command substitution (`$(...)`) runs a program
//! rather than a lookup, so there is no value to snapshot ahead of
//! execution; and the wall clock, which a statement can read (`$(date)`)
//! but which is never a session variable in the first place. Neither
//! shows up as a free variable name, so neither shows up here — this is
//! a gap in what a `${VAR}` snapshot can promise, not a defect in this
//! function.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
/// single-use materialized shell seeds from (module docs).
///
/// Called from exactly two places: `kj::gate::build_ask`, so the ask
/// records what a human is approving, and the broker's `KJ_TOOL_PLAN`
/// builder, so the lfm2d classifier hook sees the same data. Both must
/// call this rather than deriving their own free-variable set or reading
/// `context_env` directly.
///
/// A `context_env` read failure degrades to every free variable recording
/// as unset rather than failing the ask — the same shape `caller_cwd`
/// takes for a missing cwd. The snapshot is best-effort information
/// appended to an ask that has already decided to escalate; a database
/// fault here must not additionally block the human question the gate
/// exists to ask.
pub fn free_variable_values(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    context_id: ContextId,
    statements: &[kaish_kernel::PlannedStatement],
) -> Vec<AskEnvEntry> {
    let names = free_variable_names(statements);
    if names.is_empty() {
        return Vec::new();
    }

    let known: HashMap<String, String> = {
        let db = db.lock();
        match db.get_context_env(context_id) {
            Ok(rows) => rows.into_iter().map(|row| (row.key, row.value)).collect(),
            Err(e) => {
                tracing::warn!(
                    "env snapshot: could not read context_env for {}: {e} — every free \
                     variable records as unset rather than blocking the ask",
                    context_id.short()
                );
                HashMap::new()
            }
        }
    };

    names
        .into_iter()
        .map(|name| {
            let value = known.get(&name).cloned();
            AskEnvEntry { name, value }
        })
        .collect()
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
        let entries = free_variable_values(&d.kernel_db, ctx_id, &planned("echo hi"));
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

        let entries = free_variable_values(&d.kernel_db, ctx_id, &planned("echo ${FOO} ${BAZ}"));
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
        let entries = free_variable_values(&d.kernel_db, ctx_id, &planned(source));
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
        let entries = free_variable_values(&d.kernel_db, ctx_id, &planned(source));
        assert_eq!(entries.iter().filter(|e| e.name == "FOO").count(), 1);
    }
}
