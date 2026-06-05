//! `kj attach <ctx>` — pull an existing context into the current
//! session and fire the rc `attach` lifecycle on it.
//!
//! Distinct from `kj context switch`: switch is pure focus-change,
//! while attach also runs `/etc/rc/<context_type>/attach/SXX-*.{kai,md}`
//! scripts on the target. Use cases include "set up state when joining a
//! shared/team context" or "show a banner block on resume." The session
//! is moved to the target context (via `KjResult::Switch`) regardless
//! of rc script outcomes — failures land as Error blocks in the target,
//! consistent with create / fork / drift verbs.

use kaijutsu_types::ContentType;

use super::refs::{parse_context_ref, resolve_context_ref};
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_attach(
        &self,
        argv: &[String],
        caller: &KjCaller,
    ) -> KjResult {
        if argv.is_empty() {
            return KjResult::ok_ephemeral(
                self.attach_help(),
                ContentType::Markdown,
            );
        }
        let first = argv[0].as_str();
        if matches!(first, "help" | "--help" | "-h") {
            return KjResult::ok_ephemeral(
                self.attach_help(),
                ContentType::Markdown,
            );
        }

        let ctx_ref = parse_context_ref(first);
        let target_id = {
            let db = self.kernel_db().lock();
            match resolve_context_ref(&ctx_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj attach: {e}")),
            }
        };

        // Fire the rc lifecycle BEFORE returning Switch. Scripts that
        // fail land Error blocks in the target context but don't block
        // the attach — same as create / fork / drift, which prefer
        // "alive but degraded" over "rolled back."
        if let Err(e) = self
            .run_rc_lifecycle("attach", target_id, None, None, None, caller)
            .await
        {
            // run_rc_lifecycle errors today only on missing context /
            // DB read failures, not on script failures (those become
            // Error blocks). Surface as Err so the user knows the
            // attach was rejected at the lifecycle layer.
            return KjResult::Err(format!("kj attach: {e}"));
        }

        // Resolve a display label from the drift router if present.
        let label = {
            let router = self.drift_router().read();
            router
                .get(target_id)
                .and_then(|h| h.label.clone())
                .unwrap_or_else(|| target_id.short())
        };
        KjResult::Switch(target_id, format!("attached to {label}"))
    }

    fn attach_help(&self) -> String {
        r#"# kj attach — attach to an existing context

`kj attach <ctx>` brings an existing context into the current session
and fires the `attach` rc lifecycle on it. The session focus moves to
the target (same effect as `kj context switch`), and any scripts under
`/etc/rc/<context_type>/attach/SXX-*.{kai,md}` run on the target.

## Usage

- `kj attach <label>` — by label (e.g. `kj attach planner`)
- `kj attach <hex-prefix>` — by id prefix
- `kj attach .parent` — to the current context's parent

## When to use vs `kj context switch`

- `kj context switch` — pure focus change; no scripts fire.
- `kj attach` — focus change + lifecycle scripts (banner blocks,
  state refresh, audit log entries).

## Failure semantics

Script failures insert Error blocks in the target context but don't
roll back the attach — consistent with create / fork / drift verbs.
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// Caller with no joined context — kj attach should work without
    /// one (the user is attaching FROM nowhere TO something).
    fn unjoined_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: None,
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        }
    }

    fn set_context_type(d: &KjDispatcher, ctx: ContextId, ty: &str) {
        d.kernel_db()
            .lock()
            .update_context_type(ctx, ty)
            .expect("set context_type");
    }

    async fn install_attach_script(d: &KjDispatcher, ctx_type: &str, content: &str, ext: &str) {
        let path = format!("/etc/rc/{ctx_type}/attach/S00-marker.{ext}");
        install_rc_script_file(d, &path, content).await;
    }

    fn block_contents(d: &KjDispatcher, ctx: ContextId) -> Vec<String> {
        d.block_store()
            .block_snapshots(ctx)
            .map(|snaps| snaps.into_iter().map(|s| s.content).collect())
            .unwrap_or_default()
    }

    fn block_kinds(d: &KjDispatcher, ctx: ContextId) -> Vec<kaijutsu_types::BlockKind> {
        d.block_store()
            .block_snapshots(ctx)
            .map(|snaps| snaps.into_iter().map(|s| s.kind).collect())
            .unwrap_or_default()
    }

    /// Happy path: `kj attach <label>` returns Switch and runs the
    /// `attach` rc scripts on the target.
    #[tokio::test]
    async fn attach_returns_switch_and_runs_scripts() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let target = register_context(&d, Some("planner"), None, principal);
        set_context_type(&d, target, "planner");

        install_attach_script(&d, "planner", "attach-marker", "md").await;

        let caller = unjoined_caller();
        let result = d.dispatch(&[s("attach"), s("planner")], &caller).await;
        match &result {
            KjResult::Switch(id, msg) => {
                assert_eq!(*id, target);
                assert!(msg.contains("attached to"), "msg: {msg}");
            }
            other => panic!("expected Switch, got {other:?}"),
        }

        let contents = block_contents(&d, target);
        assert!(
            contents.iter().any(|c| c.contains("attach-marker")),
            "attach script must land its content; got: {contents:?}"
        );
    }

    /// `.kai` script for attach sees `KJ_VERB=attach` and `KJ_CONTEXT`
    /// matching the target. Asserts via `case` (kaish-style — see
    /// `gotcha_kaish_test_eq`).
    #[tokio::test]
    async fn attach_kai_script_sees_overlay_vars() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let target = register_context(&d, Some("watched"), None, principal);
        set_context_type(&d, target, "watched");

        install_attach_script(
            &d,
            "watched",
            // Exit non-zero if vars are missing or wrong; non-zero
            // produces an Error block.
            "test -n \"$KJ_CONTEXT\" || exit 1\ncase \"$KJ_VERB\" in attach) ;; *) exit 2 ;; esac",
            "kai",
        )
        .await;

        let caller = unjoined_caller();
        let result = d.dispatch(&[s("attach"), s("watched")], &caller).await;
        assert!(matches!(result, KjResult::Switch(..)), "result: {result:?}");

        let kinds = block_kinds(&d, target);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Error),
            "overlay-var assertions failed; kinds: {kinds:?}"
        );
    }

    /// Missing context ref returns Err, not a panic — matches the
    /// `kj context switch <missing>` behavior.
    #[tokio::test]
    async fn attach_missing_ref_errors() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d.dispatch(&[s("attach"), s("nonexistent-label")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)), "result: {result:?}");
    }

    /// No-arg `kj attach` prints help (ephemeral, content-type
    /// markdown), not an error — matches `kj rc help` style.
    #[tokio::test]
    async fn attach_no_args_prints_help() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d.dispatch(&[s("attach")], &caller).await;
        assert!(result.is_ok(), "result: {result:?}");
        assert!(
            result.message().contains("kj attach"),
            "help output should mention the command, got: {}",
            result.message()
        );
    }
}
