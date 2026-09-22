    use super::*;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ContextId, PrincipalId};

    fn admit(d: &KjDispatcher, context: ContextId) -> crate::runtime::admission::ContextAdmission {
        d.kernel().admit_context(context).expect("admit rc lifecycle")
    }

    fn test_invocation<'a>(verb: &'a str, admission: &'a crate::runtime::admission::ContextAdmission) -> RcInvocation<'a> {
        RcInvocation::new(verb, admission, &tokio_util::sync::CancellationToken::new())
    }

    #[tokio::test]
    async fn default_seed_does_not_enable_host_exec_for_later_create_scripts() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let context = register_context(&d, Some("no-host-exec"), None, PrincipalId::new());
        install_rc_script_file(
            &d,
            "/config/rc/default/create/S99-host-exec.kai",
            "/usr/bin/printf unexpected",
        )
        .await;

        let admission = admit(&d, context);
        crate::rc::run(
            &d,
            test_invocation("create", &admission),
            &caller_with_context(context),
        )
        .await
        .unwrap();

        let run = find_run_for_context(&d, context, "create").expect("create run");
        assert_eq!(run.outcome, Some(RcOutcome::Failed));
    }

    #[tokio::test]
    async fn host_exec_opt_in_enables_later_create_scripts() {
        let d = std::sync::Arc::new(test_dispatcher_rc_with_host_exec().await);
        d.set_self_arc();
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let context = register_context(&d, Some("host-exec"), None, PrincipalId::new());
        install_rc_script_file(
            &d,
            "/config/rc/default/create/S99-host-exec.kai",
            "/usr/bin/printf expected",
        )
        .await;

        let admission = admit(&d, context);
        crate::rc::run(
            &d,
            test_invocation("create", &admission),
            &caller_with_context(context),
        )
        .await
        .unwrap();

        let run = find_run_for_context(&d, context, "create").expect("create run");
        assert_eq!(run.outcome, Some(RcOutcome::Ok));
    }

    #[tokio::test]
    async fn rc_snapshots_programs_but_reads_companion_data_at_execution() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let ctx = register_context(&d, Some("snapshot"), None, PrincipalId::new());
        set_context_type(&d, ctx, "snapshot");
        install_rc_script_file(&d, "/config/rc/snapshot/create/S00-change.kai", r#"
            echo 'exit 19' > /config/rc/snapshot/create/S10-author.kai
            echo 'new data' > /config/rc/snapshot/create/S10-author.md
        "#).await;
        install_rc_script_file(&d, "/config/rc/snapshot/create/S10-author.kai", r#"
            kj block create --role system --kind text --content-type text/markdown < /config/rc/snapshot/create/S10-author.md
        "#).await;
        install_rc_script_file(&d, "/config/rc/snapshot/create/S10-author.md", "old data").await;
        let admission = admit(&d, ctx);
        crate::rc::run(&d, test_invocation("create", &admission), &caller_with_context(ctx)).await.unwrap();
        let blocks = d.block_store().block_snapshots(ctx).unwrap();
        let instructions: Vec<_> = blocks.iter().filter(|b| b.kind == BlockKind::Text).collect();
        assert_eq!(instructions.len(), 1);
        assert_eq!(instructions[0].content, "new data\n");
        let run = find_run_for_context(&d, ctx, "create").unwrap();
        assert_eq!(run.outcome, Some(RcOutcome::Ok));
        assert_eq!(run.script_count, Some(2));
    }

    #[tokio::test]
    async fn shipped_instruction_scripts_preserve_order_and_rendered_prompt() {
        use crate::vfs::VfsOps;
        for context_type in ["default", "coder", "director", "mcp", "toolie", "musician"] {
            let d = std::sync::Arc::new(test_dispatcher_rc().await);
            d.set_self_arc();
            let prefix = format!("/config/rc/{context_type}/create/");
            let mut expected = Vec::new();
            for (path, _) in crate::seed_scripts::seed_files().into_iter()
                .filter(|(path, _)| path.starts_with(&prefix) && path.ends_with(".md")) {
                let data = d.kernel().vfs().read_all(std::path::Path::new(&path)).await.unwrap();
                expected.push(String::from_utf8(data).unwrap());
            }
            assert!(!expected.is_empty(), "{context_type} must exercise companion data");
            let mut caller = console_caller(&d);
            caller.actor_id = PrincipalId::new();
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id: caller.actor_id, name: "amy".into(), created_at: 0,
                retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
            let created = d.dispatch(&argv(&["context", "create", "prompt-test", "--type", context_type]), &caller).await;
            assert!(created.is_ok(), "{context_type}: {}", created.message());
            let ctx = lookup_context_id(&d, "prompt-test");
            let blocks = d.block_store().block_snapshots(ctx).unwrap();
            let instructions: Vec<_> = blocks.iter()
                .filter(|b| b.role == Role::System && b.kind == BlockKind::Text).collect();
            let markdown: Vec<_> = instructions.iter()
                .filter(|b| b.content_type == ContentType::Markdown).map(|b| b.content.clone()).collect();
            assert_eq!(markdown, expected, "{context_type}: companion instructions changed or reordered");
            assert!(instructions.iter().all(|b| b.id.principal_id == caller.actor_id && b.status == Status::Done),
                "{context_type}: instruction attribution or status differs");
            caller.context_id = Some(ctx);
            let prompt = d.dispatch(&argv(&["context", "prompt"]), &caller).await;
            assert!(prompt.is_ok(), "{context_type}: {}", prompt.message());
            for instruction in &expected {
                assert!(prompt.message().contains(instruction.trim()), "{context_type}: missing rendered instruction");
            }
            let base = include_str!("../../../../assets/defaults/rc/lib/create/S00-base.md").trim();
            assert_eq!(prompt.message().contains(base), ["default", "coder", "director"].contains(&context_type),
                "{context_type}: optional base composition changed");
        }
    }

    /// `bassist` merged into `musician` (docs/chameleon.md, "Players — a
    /// context_type is an rc bundle"): the seed ships no `bassist` path at
    /// all, and `musician`'s `create` bundle carries the folded chair voice.
    #[test]
    fn bassist_retired_musician_carries_the_folded_chair() {
        let seeds = crate::seed_scripts::seed_files();
        assert!(
            seeds.iter().all(|(path, _)| !path.starts_with("/config/rc/bassist/")),
            "bassist must ship no seed after the merge into musician"
        );
        let chair = crate::seed_scripts::seed_body("/config/rc/musician/create/S05-chair.md")
            .expect("musician must carry the folded chair voice");
        assert!(
            chair.contains("low register"),
            "the chair's register guidance must survive the fold: {chair}"
        );
    }

    #[tokio::test]
    async fn rc_markdown_is_data_not_an_executable_entry() {
        use crate::vfs::VfsOps;
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let ctx = register_context(&d, Some("md-data"), None, PrincipalId::new());
        set_context_type(&d, ctx, "mddata");
        d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None).unwrap();
        for name in ["S00-former.md", "README.md"] {
            install_rc_script_file(&d, &format!("/config/rc/mddata/create/{name}"), "data only").await;
        }
        d.kernel().vfs().symlink(
            std::path::Path::new("/config/rc/mddata/create/S10-missing.md"),
            std::path::Path::new("missing-target.md"),
        ).await.unwrap();
        let admission = admit(&d, ctx);
        crate::rc::run(&d, test_invocation("create", &admission), &caller_with_context(ctx)).await.unwrap();
        assert!(d.block_store().block_snapshots(ctx).unwrap().is_empty());
        let run = find_run_for_context(&d, ctx, "create").unwrap();
        assert_eq!(run.script_count, Some(0));
        assert_eq!(run.outcome, Some(RcOutcome::Ok));
    }

    #[tokio::test]
    async fn rc_explicit_instructions_preserve_input_and_performer() {
        use crate::vfs::VfsOps;
        for symlinked in [false, true] {
            let d = std::sync::Arc::new(test_dispatcher_rc().await);
            d.set_self_arc();
            let creator = PrincipalId::new();
            let ctx = register_context(&d, Some("explicit"), None, creator);
            set_context_type(&d, ctx, "explicit");
            let mut caller = caller_with_context(ctx);
            caller.actor_id = PrincipalId::new();
            assert_ne!(creator, caller.principal_id);
            assert_ne!(creator, caller.actor_id);
            assert_ne!(caller.principal_id, caller.actor_id);
            let path = "/config/rc/explicit/create/S00-instructions.kai";
            // Exceeds both the agent preview and internal output limits. Stdin
            // is instruction data and must never be replaced with a preview.
            let body = format!("--literal option\n{}\n\n", "頑張（がんば）って！\n".repeat(170_000));
            install_rc_script_file(&d, &path.replace(".kai", ".md"), &body).await;
            let program = r#"kj block create --role system --kind text --content-type text/markdown < "$(dirname "$0")/$(basename "$0" .kai).md""#;
            if symlinked {
                install_rc_script_file(&d, "/config/rc/lib/create/S00-shared.kai", program).await;
                d.kernel().vfs().symlink(
                    std::path::Path::new(path),
                    std::path::Path::new("../../lib/create/S00-shared.kai"),
                ).await.expect("script symlink");
            } else {
                install_rc_script_file(&d, path, program).await;
            }
            let admission = admit(&d, ctx);
            crate::rc::run(&d, test_invocation("create", &admission), &caller).await.unwrap();
            let blocks = d.block_store().block_snapshots(ctx).unwrap();
            let instructions: Vec<_> = blocks.iter()
                .filter(|b| b.role == Role::System && b.kind == BlockKind::Text).collect();
            assert_eq!(instructions.len(), 1, "expected one instruction block; kinds: {:?}",
                blocks.iter().map(|b| (&b.kind, b.content.chars().take(2000).collect::<String>())).collect::<Vec<_>>());
            let instruction = instructions[0];
            assert_eq!(instruction.id.principal_id, caller.actor_id);
            assert_eq!(instruction.status, Status::Done);
            assert_eq!(instruction.content_type, ContentType::Markdown);
            assert_eq!(instruction.content.len(), body.len());
            assert!(instruction.content == body, "instruction bytes differ");
        }
    }

    #[tokio::test]
    async fn rc_instruction_input_errors_do_not_author_blocks() {
        use crate::vfs::VfsOps;
        for invalid_utf8 in [false, true] {
            let d = std::sync::Arc::new(test_dispatcher_rc().await);
            d.set_self_arc();
            let ctx = register_context(&d, Some("bad-input"), None, PrincipalId::new());
            set_context_type(&d, ctx, "badinput");
            let path = "/config/rc/badinput/create/S00-instructions.kai";
            install_rc_script_file(&d, path,
                r#"kj block create --role system --kind text --content-type text/markdown < "$0.txt""#,
            ).await;
            if invalid_utf8 {
                d.kernel().vfs().write_all(std::path::Path::new(&format!("{path}.txt")),
                    &[b'a', 0xff, b'\n']).await.unwrap();
            }
            let admission = admit(&d, ctx);
            crate::rc::run(&d, test_invocation("create", &admission), &caller_with_context(ctx)).await.unwrap();
            let blocks = d.block_store().block_snapshots(ctx).unwrap();
            assert!(!blocks.iter().any(|b| b.kind == BlockKind::Text));
            assert!(blocks.iter().any(|b| b.kind == BlockKind::Error), "input failure must be visible");
            let run = find_run_for_context(&d, ctx, "create").unwrap();
            assert_eq!(run.outcome, Some(RcOutcome::Failed));
        }
    }

    #[tokio::test]
    async fn rc_instruction_content_precedence_and_empty_input() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let ctx = register_context(&d, Some("input-precedence"), None, PrincipalId::new());
        set_context_type(&d, ctx, "precedence");
        install_rc_script_file(&d, "/config/rc/precedence/create/S00-instructions.kai", r#"
            echo ignored | kj block create --role system --kind text --content 'explicit'
            kj block create --role system --kind text < "$0.txt"
        "#).await;
        install_rc_script_file(&d, "/config/rc/precedence/create/S00-instructions.kai.txt", "").await;
        let admission = admit(&d, ctx);
        crate::rc::run(&d, test_invocation("create", &admission), &caller_with_context(ctx)).await.unwrap();
        let blocks = d.block_store().block_snapshots(ctx).unwrap();
        let instructions: Vec<_> = blocks.iter().filter(|b| b.kind == BlockKind::Text).collect();
        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[0].content, "explicit");
        assert_eq!(instructions[1].content, "");
        assert!(instructions.iter().all(|b| b.content_type == ContentType::Plain));
    }

    #[tokio::test]
    async fn unknown_lifecycle_verb_is_an_error() {
        let d = test_dispatcher().await;
        let caller = console_caller(&d);
        let context = register_context(&d, Some("unknown-verb"), None, caller.principal_id);
        let admission = admit(&d, context);
        let result = crate::rc::run(
            &d,
            test_invocation("cretae", &admission),
            &caller,
        ).await;
        assert!(result.is_err(), "an unknown lifecycle verb must not succeed without running");
        assert!(result.unwrap_err().contains("cretae"));
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// A privileged caller joined to a fresh parentless console context,
    /// registered directly in the db the way boot's `ensure_root_contexts`
    /// registers a root console. `kj context create` needs a parent, so a
    /// create from this caller lands as the console's child. Privileged so
    /// these rc-lifecycle tests can `kj context create` (Operator-gated) as
    /// the trusted control plane would.
fn console_caller(d: &KjDispatcher) -> KjCaller {
        let principal_id = PrincipalId::new();
        let console = register_context(d, None, None, principal_id);
        KjCaller {
            principal_id,
            actor_id: principal_id,
            reviewer_id: None,
            context_id: Some(console),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn block_kinds_in(dispatcher: &KjDispatcher, ctx: ContextId) -> Vec<kaijutsu_types::BlockKind> {
        dispatcher
            .block_store()
            .block_snapshots(ctx)
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.kind)
            .collect()
    }

    fn block_contents_in(dispatcher: &KjDispatcher, ctx: ContextId) -> Vec<String> {
        dispatcher
            .block_store()
            .block_snapshots(ctx)
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.content)
            .collect()
    }

    /// Resolve a context by label so tests don't have to scrape the
    /// "created context 'X' (id)" message.
    fn lookup_context_id(dispatcher: &KjDispatcher, label: &str) -> ContextId {
        let db = dispatcher.kernel_db().lock();
        db.find_context_by_label(label)
            .expect("get_context_by_label")
            .expect("context exists")
            .context_id
    }

    #[tokio::test]
    async fn rc_create_script_authors_instruction_block() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-prompt.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'You are a test context. Be terse.'"#).await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-md", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-md");
        let contents = block_contents_in(&d, new_id);
        assert!(
            contents.iter().any(|c| c.contains("You are a test context")),
            "expected authored instruction, got: {contents:?}"
        );
    }

    /// A composed executable follows its symlink and authors the instruction.
    #[tokio::test]
    async fn rc_create_follows_symlinked_script() {
        use crate::vfs::VfsOps;
        // `/config/rc` is an ordinary host directory (`LocalBackend`) — a
        // real POSIX symlink, resolved host-relative to the link's own
        // directory, same shape `reseed_rc_files` writes for the embedded
        // seed's init.d composition.
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        // The shared, canonical stance lives once under a `lib` type.
        install_rc_script_file(
            &d,
            "/config/rc/lib/create/S00-shared.kai",
            r#"kj block create --role system --kind text --content-type text/markdown --content 'You are composed from a shared stance fragment. Be terse.'"#,
        )
        .await;
        // The consuming type composes it in by symlink.
        d.kernel()
            .vfs()
            .symlink(
                std::path::Path::new("/config/rc/test/create/S00-stance.kai"),
                std::path::Path::new("../../lib/create/S00-shared.kai"),
            )
            .await
            .expect("create rc symlink");

        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-link", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-link");
        let contents = block_contents_in(&d, new_id);
        assert!(
            contents
                .iter()
                .any(|c| c.contains("composed from a shared stance fragment")),
            "expected symlinked script to author an instruction, got: {contents:?}"
        );
    }

    /// A composed instruction link that cannot be read is corruption, not an
    /// empty prompt section. Creation leaves the context inert, reports the
    /// repair path, and records the unreadable link in a durable Error block.
    #[tokio::test]
    async fn rc_create_reports_and_records_a_broken_symlinked_script() {
        use crate::vfs::VfsOps;

        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        d.kernel()
            .vfs()
            .symlink(
                std::path::Path::new("/config/rc/test/create/S00-broken.kai"),
                std::path::Path::new("../../lib/create/no-such-shared.kai"),
            )
            .await
            .expect("create broken rc symlink");

        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-broken-link", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(
            !result.is_ok(),
            "failed discovery must report the retained diagnostic context: {}",
            result.message()
        );
        assert!(
            result.message().contains("committed") && result.message().contains("S00-broken.kai"),
            "creation must name committed state and the lifecycle fault: {}",
            result.message()
        );
        let new_id = lookup_context_id(&d, "ctx-broken-link");
        assert_eq!(
            d.has_usable_loadout(new_id),
            Ok(false),
            "a failed rc create must not leave a usable loadout"
        );
        let errors: Vec<_> = d
            .block_store()
            .block_snapshots(new_id)
            .expect("read diagnostic blocks")
            .into_iter()
            .filter(|block| block.kind == kaijutsu_types::BlockKind::Error)
            .collect();
        assert!(
            errors.iter().any(|block| block.content.contains("S00-broken.kai")),
            "the durable Error must name the unreadable link: {errors:?}"
        );
    }

    #[tokio::test]
    async fn rc_create_kai_runs_script() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-noop.kai", "true").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-kai", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-kai");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Error),
            "successful .kai should not insert error block, got kinds: {kinds:?}"
        );
    }

    /// A successful `.kai` script that prints to stdout lands its output
    /// in a `BlockKind::Trace` block (model-hidden, operator-visible).
    /// Silent scripts must NOT produce a Trace block — only emit when
    /// there's something to capture.
    #[tokio::test]
    async fn rc_kai_stdout_captured_as_trace_block() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-echo.kai", "echo \"hello from rc\"").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-echo", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-echo");
        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace_blocks: Vec<_> = snapshots
            .iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .collect();
        assert_eq!(
            trace_blocks.len(),
            1,
            "expected 1 trace block from echoing script; kinds: {:?}",
            snapshots.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
        let body = &trace_blocks[0].content;
        assert!(
            body.contains("hello from rc"),
            "trace block must contain the stdout, got: {body}"
        );
        assert!(
            body.contains("S00-echo.kai") || body.contains("S00"),
            "trace block must reference the script path or sort key, got: {body}"
        );

        // The fast path, asserted on the very block most likely to regress it:
        // an rc script with no escape bytes must leave the block exactly as it
        // was before ANSI ingest existed — no spans, no tag, no row.
        assert!(trace_blocks[0].style_spans.is_empty(), "escape-free rc output must have no spans");
        assert!(trace_blocks[0].provenance.is_none(), "escape-free rc output must stay untagged");
        assert_eq!(
            d.kernel_db()
                .lock()
                .get_block_provenance(&trace_blocks[0].id, kaijutsu_ansi::TRANSFORM_NAME)
                .expect("provenance query"),
            None,
            "escape-free rc output must not write a provenance row"
        );
    }

    #[test]
    fn rc_retains_both_streams_beyond_the_diagnostic_tail() {
        crate::spawn_kaish_thread("rc-output-test", || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
                .block_on(rc_retains_both_streams_beyond_the_diagnostic_tail_body());
        }).unwrap().join().unwrap();
    }

    async fn rc_retains_both_streams_beyond_the_diagnostic_tail_body() {
        for failed in [false, true] {
            let d = std::sync::Arc::new(test_dispatcher_rc().await);
            d.set_self_arc();
            let context = register_context(&d, Some("full-rc-output"), None, PrincipalId::new());
            set_context_type(&d, context, "full-output");
            let stdout = format!("stdout-first-{}-stdout-last", "音".repeat(3000));
            let stderr = format!("stderr-first-{}-stderr-last", "err".repeat(3000));
            let ending = if failed { "false" } else { "true" };
            install_rc_script_file(&d, "/config/rc/full-output/create/S00-output.kai",
                &format!("printf '%s' '{stdout}'; printf '%s' '{stderr}' >&2; {ending}")).await;
            let admission = admit(&d, context);
            crate::rc::run(&d, test_invocation("create", &admission), &caller_with_context(context))
                .await.unwrap();
            let blocks = d.block_store().block_snapshots(context).unwrap();
            let capture = blocks.iter().find(|block| block.kind == if failed { BlockKind::Error } else { BlockKind::Trace })
                .expect("rc output must be retained");
            assert!(capture.content.contains(&stdout), "complete stdout must survive rc capture (failed={failed})");
            assert!(capture.content.contains(&stderr), "complete stderr must survive rc capture (failed={failed})");
        }
    }

    #[test]
    fn rc_settlement_fault_retains_output_and_never_repeats_source() {
        crate::spawn_kaish_thread("rc-settlement-test", || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
                .block_on(async {
                    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
                    for column in ["result_json", "projected_at"] {
                        let d = std::sync::Arc::new(test_dispatcher_rc().await);
                        d.set_self_arc();
                        let context = register_context(&d, Some("rc-settlement-fault"), None, PrincipalId::new());
                        set_context_type(&d, context, "settlement-fault");
                        install_rc_script_file(&d, "/config/rc/settlement-fault/create/S00-once.kai",
                            "kj block create --role system --kind text --content executed-once; while test ! -e /config/rc/release-output; do sleep 0.01; done; echo retained-after-storage-fault").await;
                        install_rc_script_file(&d, "/config/rc/settlement-fault/create/S10-later.kai",
                            "kj block create --role system --kind text --content later-must-not-run").await;
                        let admission = admit(&d, context);
                        let caller = caller_with_context(context);
                        let run = crate::rc::run(&d, test_invocation("create", &admission), &caller);
                        tokio::pin!(run);
                        let mut inspect = tokio::time::interval(std::time::Duration::from_millis(5));
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                tokio::select! {
                                    result = &mut run => panic!("rc ended before fault injection: {result:?}"),
                                    _ = inspect.tick() => {
                                        if block_contents_in(&d, context).iter().any(|body| body == "executed-once") { break; }
                                    }
                                }
                            }
                        }).await.expect("script must commit its effect before fault injection");
                        d.kernel_db().lock().conn_for_ledger().authorizer(Some(move |auth: AuthContext<'_>| {
                            match auth.action {
                                AuthAction::Update { table_name: "rc_run_scripts", column_name } if column_name == column => Authorization::Deny,
                                _ => Authorization::Allow,
                            }
                        })).unwrap();
                        install_rc_script_file(&d, "/config/rc/release-output", "release").await;
                        let result = tokio::time::timeout(std::time::Duration::from_secs(3), &mut run).await.unwrap();
                        assert!(result.is_err(), "{column} fault must stop the lifecycle");
                        assert!(d.kernel().shutdown_runtime_worker().await.is_err(), "unsettled output must make shutdown fail");
                        d.kernel_db().lock().conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();
                        let contents = if column == "projected_at" {
                            assert!(d.kernel().shutdown_runtime_worker().await.is_err(),
                                "a failed block journal transaction requires document recovery");
                            let db = d.kernel_db().clone();
                            let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
                            let blocks = crate::block_store::shared_block_store_with_db(db.clone(), workspace, PrincipalId::system());
                            let recovered = crate::rc::settlement::RcSettlements::new(db, blocks.clone());
                            recovered.recover().expect("restart projects retained output without execution");
                            recovered.recover().expect("recovery is idempotent");
                            blocks.block_snapshots(context).unwrap().into_iter().map(|block| block.content).collect::<Vec<_>>()
                        } else {
                            d.kernel().shutdown_runtime_worker().await.expect("repair retries settlement without execution");
                            d.kernel().shutdown_runtime_worker().await.expect("settlement retry is idempotent");
                            block_contents_in(&d, context)
                        };
                        assert_eq!(contents.iter().filter(|body| *body == "executed-once").count(), 1);
                        assert_eq!(contents.iter().filter(|body| body.contains("retained-after-storage-fault")).count(), 1);
                        assert!(!contents.iter().any(|body| body == "later-must-not-run"));
                        let row = find_run_for_context(&d, context, "create").unwrap();
                        assert_eq!(row.outcome, Some(RcOutcome::Failed), "retry must preserve the lifecycle's failed settlement");
                    }
                });
        }).unwrap().join().unwrap();
    }

    fn check_rc_bookkeeping_admission(stage: &'static str) {
        crate::spawn_kaish_thread("rc-record-test", move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
                .block_on(async move {
                    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
                    let d = std::sync::Arc::new(test_dispatcher_rc().await);
                    d.set_self_arc();
                    let context = register_context(&d, Some("rc-record-refusal"), None, PrincipalId::new());
                    set_context_type(&d, context, "record-refusal");
                    install_rc_script_file(&d, "/config/rc/record-refusal/create/S00-mutation.kai",
                        "kj block create --role system --kind text --content must-not-execute-unrecorded").await;
                    let admission = admit(&d, context);
                    d.kernel_db().lock().conn_for_ledger().authorizer(Some(move |auth: AuthContext<'_>| {
                        let denied = match auth.action {
                            AuthAction::Insert { table_name } => table_name == stage,
                            AuthAction::Update { table_name: "rc_runs", column_name: "script_count" } => stage == "script_count",
                            _ => false,
                        };
                        if denied { Authorization::Deny } else { Authorization::Allow }
                    })).unwrap();
                    let result = crate::rc::run(&d, test_invocation("create", &admission), &caller_with_context(context)).await;
                    d.kernel_db().lock().conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();
                    assert!(result.is_err(), "a {stage} write failure must be returned");
                    let blocks = d.block_store().block_snapshots(context).unwrap_or_default();
                    assert!(!blocks.iter().any(|block| block.content == "must-not-execute-unrecorded"),
                        "{stage} must be durable before source execution");
                });
        }).unwrap().join().unwrap();
    }

    #[test]
    fn rc_bookkeeping_start_failure_prevents_execution() { check_rc_bookkeeping_admission("rc_runs"); }

    #[test]
    fn rc_bookkeeping_count_failure_prevents_execution() { check_rc_bookkeeping_admission("script_count"); }

    #[test]
    fn rc_bookkeeping_body_failure_prevents_execution() { check_rc_bookkeeping_admission("script_bodies"); }

    #[test]
    fn rc_bookkeeping_script_failure_prevents_execution() { check_rc_bookkeeping_admission("rc_run_scripts"); }

    /// The lifecycle resolves `context_type` before any script runs
    /// (`row.context_type`, read once in `rc::run`) but never
    /// told `.kai` scripts what it found. `KJ_CONTEXT_TYPE` closes that gap so
    /// a shared rc bucket can branch on it:
    /// `case "$KJ_CONTEXT_TYPE" in coder) ... esac`.
    #[tokio::test]
    async fn rc_kai_receives_context_type() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-echo-type.kai", "echo \"type=$KJ_CONTEXT_TYPE\"").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-type", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-type");
        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("the echoing script must produce a trace block");
        assert!(
            trace.content.contains("type=test"),
            "KJ_CONTEXT_TYPE must carry the resolved context_type into the .kai env, got: {}",
            trace.content
        );
    }

    /// The RC boot aesthetic (docs/ansi-and-beyond.md): an rc script that
    /// prints SGR-colored output lands a *clean* Trace block plus the span map
    /// and the original bytes. The standing invariant is asserted directly —
    /// `strip(original) == (content, style_spans)` — because it is what
    /// `kj block reproject` and the CI sweep both depend on.
    ///
    /// Note what the original is: the whole assembled block body, header lines
    /// included, not just the script's stdout. Span offsets address block
    /// content, so the transform has to run over the same string the block
    /// stores.
    #[tokio::test]
    async fn rc_kai_ansi_output_lands_clean_text_spans_and_provenance() {
        // `test_dispatcher_rc`, not `test_dispatcher`: provenance is a row in
        // the kernel db, and only the rc dispatcher's block store is DB-backed.
        // (A db-less store no-ops `store_provenance` — the same graceful
        // degradation a replica store gets — which would make this test assert
        // nothing about the row.)
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-color.kai", // A literal ESC byte in the script source — the classic
            // `[ OK ]`-in-green boot line, in miniature.
            "echo \"[ \u{1b}[32mOK\u{1b}[0m ] booted\"").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-color", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-color");
        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("colored script must still produce a trace block");

        assert!(
            trace.content.contains("[ OK ] booted"),
            "block content must be the stripped projection, got: {:?}",
            trace.content
        );
        assert!(
            !trace.content.contains('\u{1b}'),
            "no escape byte may survive into block content: {:?}",
            trace.content
        );
        assert!(!trace.style_spans.is_empty(), "the green OK must produce at least one span");
        let tag = trace.provenance.as_ref().expect("styled block must carry a provenance tag");
        assert_eq!(tag.transform, kaijutsu_ansi::TRANSFORM_NAME);
        assert_eq!(tag.version, kaijutsu_ansi::PARSER_VERSION);

        let (version, original) = d
            .kernel_db()
            .lock()
            .get_block_provenance(&trace.id, kaijutsu_ansi::TRANSFORM_NAME)
            .expect("provenance query")
            .expect("a tagged block must have a row — record() writes the row first");
        assert_eq!(version, kaijutsu_ansi::PARSER_VERSION);
        assert!(
            original.contains(&0x1bu8),
            "the stored original must be the pre-strip bytes, escapes and all"
        );

        // The standing invariant.
        assert_eq!(
            kaijutsu_ansi::strip(&original),
            (trace.content.clone(), trace.style_spans.clone()),
            "strip(original) must reproduce (content, style_spans) exactly"
        );
    }

    /// The musician transport seam: `rc::run` must seed the
    /// extra vars into the `.kai` env so a `tick` script can read `$TICK` /
    /// `$PHRASE` / `$TEMPO` and compose the turn's transport report. Echoes the
    /// vars and asserts they round-trip through the captured Trace block.
    #[tokio::test]
    async fn rc_lifecycle_with_vars_seeds_kai_env() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/tick/S00-report.kai", "echo \"tick=$TICK phrase=$PHRASE tempo=$TEMPO\"")
        .await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-tick", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-tick");

        let vars: std::collections::HashMap<String, String> = [
            ("TICK".to_string(), "128".to_string()),
            ("PHRASE".to_string(), "8".to_string()),
            ("TEMPO".to_string(), "120".to_string()),
        ]
        .into_iter()
        .collect();
        let admission = admit(&d, new_id);
        crate::rc::run(
            &d,
            crate::rc::RcInvocation {
                vars: vars.clone(),
                ..test_invocation("tick", &admission)
            },
            &caller,
        )
            .await
            .expect("tick lifecycle");

        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("the echoing tick script produced a trace block");
        assert!(
            trace.content.contains("tick=128 phrase=8 tempo=120"),
            "heartbeat vars must reach the .kai env, got: {}",
            trace.content
        );
    }

    /// Non-vacuity guard for the seam above: with NO extra vars, the same script
    /// sees empty `$TICK`/`$PHRASE`/`$TEMPO` — proving the assertion pins the
    /// seeding, not an always-populated env.
    #[tokio::test]
    async fn rc_lifecycle_without_vars_leaves_heartbeat_empty() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/tick/S00-report.kai", "echo \"tick=$TICK phrase=$PHRASE tempo=$TEMPO\"")
        .await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-novars", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-novars");

        // The plain lifecycle (no extra vars) leaves the heartbeat unset.
        let admission = admit(&d, new_id);
        crate::rc::run(
            &d,
            test_invocation("tick", &admission),
            &caller,
        )
            .await
            .expect("tick lifecycle");

        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("the echoing tick script produced a trace block");
        assert!(
            trace.content.contains("tick= phrase= tempo="),
            "without seeded vars the heartbeat must be empty, got: {}",
            trace.content
        );
    }

    /// Lifecycle Trace diagnostics retain context-creator attribution.
    #[tokio::test]
    async fn rc_kai_trace_block_is_authored_by_context_owner_not_caller() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/tick/S00-report.kai", "echo \"the beat goes on\"")
        .await;

        let owner = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-owned-kai", "--type", "test"]),
                &owner,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-owned-kai");

        let visitor = console_caller(&d);
        assert_ne!(
            owner.principal_id, visitor.principal_id,
            "fixture must use two distinct principals or the assertion is vacuous"
        );

        let admission = admit(&d, new_id);
        crate::rc::run(
            &d,
            test_invocation("tick", &admission),
            &visitor,
        )
            .await
            .expect("tick lifecycle");

        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("the echoing script produced a trace block");
        assert_eq!(
            trace.id.principal_id, owner.principal_id,
            "rc Trace block must belong to the context owner"
        );
        assert_ne!(
            trace.id.principal_id, visitor.principal_id,
            "rc Trace block must NOT be smeared with the triggering caller"
        );
    }

    #[tokio::test]
    async fn rc_nested_context_keeps_the_lead_as_director_and_its_reviewer() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-spawn-coder.kai", "kj context create child-work --type child --as coder")
        .await;
        install_rc_script_file(&d, "/config/rc/child/create/S00-noop.kai", "true")
        .await;

        let mut caller = console_caller(&d);
        let lead = PrincipalId::new();
        let coder = PrincipalId::new();
        for (principal_id, name) in [
            (caller.principal_id, "amy"),
            (lead, "lead"),
            (coder, "coder"),
        ] {
            d.kernel_db()
                .lock()
                .insert_character(&crate::kernel_db::CharacterRow {
                    principal_id,
                    name: name.into(),
                    created_at: 0,
                    retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                })
                .expect("insert live character");
        }
        caller.actor_id = lead;
        caller.reviewer_id = Some(caller.principal_id);

        let result = d
            .dispatch(
                &argv(&["context", "create", "lead-work", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "parent create failed: {}", result.message());

        let child_id = lookup_context_id(&d, "child-work");
        let child = d
            .kernel_db()
            .lock()
            .get_context(child_id)
            .expect("read child")
            .expect("rc created child context");
        assert_eq!(child.created_by, caller.principal_id, "Amy requested the work");
        assert_eq!(child.played_by, Some(coder));
        assert_eq!(child.director_id, Some(lead), "the lead directs its coder");
        assert_eq!(child.reviewer_id, None, "there is no implicit reviewer override");
        assert_eq!(
            d.kernel().resolve_context_review(child_id).await.unwrap().reviewer.principal_id,
            lead,
            "the child is accountable to the context the lead created it from, so the lead reviews it",
        );
    }

    /// Failure blocks are the loudest thing rc writes, so they are the worst
    /// place to name the wrong principal — a reader chasing "who broke this
    /// context" would find the visitor.
    #[tokio::test]
    async fn rc_failure_block_is_authored_by_context_owner_not_caller() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/tick/S00-broken.zzz", "this extension has no handler")
        .await;
        // `.zzz` is filtered out by the loader; use a `.kai` that exits nonzero
        // to reach the failure path through a supported extension.
        install_rc_script_file(&d, "/config/rc/test/tick/S01-fail.kai", "exit 3")
        .await;

        let owner = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-owned-fail", "--type", "test"]),
                &owner,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-owned-fail");

        let visitor = console_caller(&d);
        assert_ne!(owner.principal_id, visitor.principal_id);

        let admission = admit(&d, new_id);
        crate::rc::run(
            &d,
            test_invocation("tick", &admission),
            &visitor,
        )
            .await
            .expect("tick lifecycle");

        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let failure = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Error)
            .expect("the failing script produced an error block");
        assert_eq!(
            failure.id.principal_id, owner.principal_id,
            "rc failure block must belong to the context owner"
        );
        assert_ne!(
            failure.id.principal_id, visitor.principal_id,
            "rc failure block must NOT be smeared with the triggering caller"
        );
    }

    /// Helper: seed a child context with one block so the default hydration
    /// marker (`last_block_id`) resolves, then run the fork lifecycle with the
    /// given fork kind against the REAL shipped musician fork-hydrate script.
    /// Returns the child's hydration policy after the lifecycle.
    async fn run_musician_fork_hydrate(
        fork_kind: ForkKind,
    ) -> (
        std::sync::Arc<KjDispatcher>,
        ContextId,
        Option<(kaijutsu_types::BlockId, u32)>,
    ) {
        // Arc + set_self_arc so the .kai script can reach the `kj` builtin
        // (the script runs `kj context hydrate`); see `rc_kai_can_call_kj`.
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        // Install the real shipped script under a test type so the fork verb
        // dispatches it — pins the test to the actual seeded body, not a copy.
        install_rc_script_file(
            &d,
            "/config/rc/test/fork/S40-hydrate.kai",
            include_str!("../../../../assets/defaults/rc/musician/fork/S40-hydrate.kai"),
        )
        .await;

        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let child = register_context(&d, Some("child"), Some(parent), principal);
        set_context_type(&d, child, "test");
        // The child needs a block for the default prefix marker to resolve.
        d.block_store()
            .create_document(child, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                child,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "seed".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(principal),
            )
            .unwrap();

        let caller = caller_with_context(child);
        let admission = admit(&d, child);
        crate::rc::run(
            &d,
            crate::rc::RcInvocation {
                parent: Some(parent),
                fork_kind: Some(fork_kind),
                ..test_invocation("fork", &admission)
            },
            &caller,
        )
            .await
            .expect("fork lifecycle");
        // The script must not have errored (e.g. a denied/failed `kj` call).
        assert!(
            !block_kinds_in(&d, child).contains(&BlockKind::Error),
            "fork-hydrate script errored: {:?}",
            block_contents_in(&d, child)
        );
        let policy = d.kernel_db().lock().get_hydration_policy(child).unwrap();
        (d, child, policy)
    }

    /// A THIN fork (shallow) is the player-spawn path: the fork-hydrate script
    /// re-establishes the window on the lean child (it would otherwise drive at
    /// tempo with full history — the create-side script doesn't run on fork).
    #[tokio::test]
    async fn musician_fork_hydrate_windows_a_thin_fork() {
        let (_d, _child, policy) = run_musician_fork_hydrate(ForkKind::Filtered).await;
        match policy {
            Some((_marker, window)) => assert_eq!(window, 16, "thin fork gets the --window 16 guard"),
            None => panic!("a thin (shallow) fork must set a hydration window"),
        }
    }

    /// A FULL fork (regular `kj fork`) is the take-it-all / new-KV-cache path,
    /// not a player spawn — windowing is a thin-fork concern, so the script
    /// leaves a full fork un-windowed (full history stays live, no policy set).
    #[tokio::test]
    async fn musician_fork_hydrate_skips_a_full_clone() {
        let (_d, _child, policy) = run_musician_fork_hydrate(ForkKind::Full).await;
        assert!(
            policy.is_none(),
            "a full clone must NOT be windowed (it would pin the whole inherited log); got {policy:?}"
        );
    }

    #[tokio::test]
    async fn rc_kai_silent_success_inserts_no_trace_block() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-silent.kai", "true").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-silent", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-silent");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Trace),
            "silent script must not produce trace block, got kinds: {kinds:?}"
        );
    }

    /// Trace blocks have `Role::System` but `BlockKind::Trace` — the
    /// hydrator must skip them so the model never sees rc operator
    /// telemetry. (Belt-and-suspenders against a future regression that
    /// widens the System-role carve-out to all kinds.)
    #[tokio::test]
    async fn rc_kai_trace_block_is_hidden_from_llm_hydrate() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-echo.kai", "echo MODEL_MUST_NOT_SEE_THIS").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-hidden", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-hidden");
        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        // Sanity: the trace block exists in the document.
        assert!(
            snapshots
                .iter()
                .any(|b| b.kind == kaijutsu_types::BlockKind::Trace
                    && b.content.contains("MODEL_MUST_NOT_SEE_THIS")),
            "trace block with sentinel must exist in document"
        );
        // The hydrator must not surface it.
        let msgs = crate::llm::hydrate_from_blocks(&snapshots);
        for m in &msgs {
            let rendered = format!("{:?}", m);
            assert!(
                !rendered.contains("MODEL_MUST_NOT_SEE_THIS"),
                "trace content leaked into hydrated message: {rendered}"
            );
        }
    }

    /// End-to-end: a slow rc `.kai` script must time out, terminate any
    /// running child, and land a failure block — without hanging the
    /// `context create` RPC reply. Exercises the full chain:
    ///   `kaijutsu_types::TimeoutPolicy`
    ///     → `Kernel::timeouts()`
    ///     → `EmbeddedKaish::with_identity` (kaish KernelConfig::request_timeout)
    ///     → `run_kai_script` (per-call ExecuteOptions::with_timeout)
    ///     → `kaish::Kernel::execute_with_options` (124 + wait_or_kill)
    ///     → `insert_rc_failure_block`.
    #[tokio::test]
    async fn rc_kai_script_timeout_inserts_failure_block() {
        let policy = kaijutsu_types::TimeoutPolicy {
            rc_script_timeout: std::time::Duration::from_millis(150),
            ..Default::default()
        };
        let d = std::sync::Arc::new(test_dispatcher_with_timeouts(policy).await);
        d.set_self_arc();

        // Sleep well past the 150ms bound so the timeout MUST fire. The
        // kaish `sleep` builtin honors `ctx.cancel`, so the timer-induced
        // cancel surfaces as exit 130; the kernel then maps the elapsed
        // timeout to exit 124 with a "timed out" message in stderr.
        install_rc_script_file(&d, "/config/rc/test/create/S00-slow.kai", "sleep 10").await;

        let caller = console_caller(&d);
        let started = std::time::Instant::now();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-slow", "--type", "test"]),
                &caller,
            )
            .await;
        let elapsed = started.elapsed();

        // Context creation succeeded (rc failures don't block creation —
        // the new context is "alive but degraded" per the SysV-style
        // semantics documented in lifecycle.rs).
        assert!(
            result.is_ok(),
            "context create should succeed even when rc script times out: {}",
            result.message()
        );

        // Did NOT block 10 seconds waiting for sleep — the timeout cut in.
        // Generous upper bound to absorb CI jitter; the actual budget is ~150ms.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "rc timeout must not block context create: elapsed={:?}",
            elapsed
        );

        // The new context now carries an Error block describing the timeout.
        let new_id = lookup_context_id(&d, "ctx-slow");
        let snapshots = d
            .block_store()
            .block_snapshots(new_id)
            .expect("block_snapshots");
        let error_blocks: Vec<_> = snapshots
            .iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Error)
            .collect();
        assert_eq!(
            error_blocks.len(),
            1,
            "expected exactly one error block, got {}: kinds={:?}",
            error_blocks.len(),
            snapshots.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
        let body = &error_blocks[0].content;
        assert!(
            body.contains("S00-slow.kai") || body.contains("slow"),
            "error block should reference the failing script path: {body}"
        );
        assert!(
            body.to_lowercase().contains("timed out") || body.contains("124"),
            "error block should mention timeout (exit 124 or 'timed out'): {body}"
        );
    }

    #[tokio::test]
    async fn rc_kai_can_call_kj() {
        // .kai scripts get `kj` registered when the dispatcher's
        // self-Arc is wired. Without `set_self_arc`, the test
        // dispatcher's scripts still run but can't reach kj.
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();

        // Script asserts overlay vars are populated and that `kj` is
        // callable. Exit 0 → no error block; non-zero → error block.
        install_rc_script_file(&d, "/config/rc/test/create/S00-introspect.kai", "[[ -n \"$KJ_CONTEXT\" ]] && [[ -n \"$KJ_VERB\" ]] && kj context list").await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-kj", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-kj");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Error),
            ".kai with kj invocation must exit 0 — got error block; kinds: {kinds:?}"
        );
    }

    /// `context_type = "nonexistent"` has an rc bucket (`/config/rc/nonexistent/`,
    /// which `context create` requires) but no `create/` verb directory under
    /// `test_dispatcher()`'s host-backed `LocalBackend` mount. `dispatch`'s own `Ok` and an empty
    /// context alone don't distinguish "load_scripts correctly saw zero
    /// scripts" from "load_scripts errored and `context.rs` swallowed it"
    /// (`context create` logs-and-continues on an rc-lifecycle `Err`, per the
    /// comment at its call site) — both leave the same block-free context and
    /// the same `Ok` dispatch result. The run row's typed outcome is what
    /// actually tells them apart, so assert on it directly.
    #[tokio::test]
    async fn rc_no_scripts_for_type_is_noop() {
        use crate::vfs::VfsOps;
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        d.kernel()
            .vfs()
            .mkdir(std::path::Path::new("/config/rc/nonexistent"), 0o755)
            .await
            .expect("an rc bucket with no verbs");
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-empty", "--type", "nonexistent"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-empty");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            kinds.is_empty(),
            "no scripts should leave context block-free, got: {kinds:?}"
        );

        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert!(run.finished_at.is_some());
        assert_eq!(
            run.outcome,
            Some(RcOutcome::Ok),
            "a genuinely absent rc directory is zero scripts, not a failed run"
        );
    }

    /// The run log records how many scripts the run intended to execute, so
    /// a reader can tell a run that stopped early from one where a script
    /// failed. Both outcomes are `Failed`; only the count separates them.
    #[tokio::test]
    async fn rc_run_records_intended_script_count() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/counted/create/S00-one.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'first'"#).await;
        install_rc_script_file(&d, "/config/rc/counted/create/S10-two.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'second'"#).await;

        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-counted", "--type", "counted"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-counted");
        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert_eq!(run.outcome, Some(RcOutcome::Ok));
        assert_eq!(
            run.script_count,
            Some(2),
            "the run log must say how many scripts were intended"
        );
    }

    /// A verb with no scripts records a count of zero rather than leaving it
    /// unset: zero-of-zero is a complete run, and only a run that failed
    /// before the script list loaded should read as having no count at all.
    #[tokio::test]
    async fn rc_empty_verb_records_zero_script_count() {
        use crate::vfs::VfsOps;
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let vfs = d.kernel().vfs();
        vfs.mkdir(std::path::Path::new("/config/rc/nothinghere"), 0o755)
            .await
            .expect("an rc bucket");
        vfs.mkdir(std::path::Path::new("/config/rc/nothinghere/create"), 0o755)
            .await
            .expect("an empty create verb");
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-zero", "--type", "nothinghere"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-zero");
        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert_eq!(run.script_count, Some(0));
    }

    /// A noncanonical .kai filename fails discovery before any script runs.
    #[tokio::test]
    async fn rc_non_canonical_script_name_fails_the_verb() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(
            &d,
            "/config/rc/stray/create/S00-benign.kai",
            "echo would-run",
        )
        .await;
        // The shape that prompted this: a hook body parked beside its
        // installer, named so it is data rather than a script.
        install_rc_script_file(&d, "/config/rc/stray/create/guard.hook.kai", "exit 0").await;

        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-stray", "--type", "stray"]),
                &caller,
            )
            .await;
        assert!(!result.is_ok(), "invalid discovery must report partial completion: {}", result.message());
        assert!(result.message().contains("committed"), "{}", result.message());

        let new_id = lookup_context_id(&d, "ctx-stray");
        let kinds = block_kinds_in(&d, new_id);
        assert_eq!(
            kinds,
            vec![kaijutsu_types::BlockKind::Error],
            "the verb records its failure without running any script"
        );
        assert!(block_contents_in(&d, new_id).iter().any(|body| body.contains("guard.hook.kai")));

        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert_eq!(
            run.outcome,
            Some(RcOutcome::Failed),
            "a non-canonical script name is a failed run, not a silent skip"
        );
    }

    /// Non-executable data files do not participate in lifecycle dispatch.
    #[tokio::test]
    async fn rc_ignores_files_that_are_not_scripts() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/inert/create/S00-real.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'the real script'"#).await;
        install_rc_script_file(&d, "/config/rc/inert/create/README.txt", "notes").await;

        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-inert", "--type", "inert"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-inert");
        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert_eq!(run.outcome, Some(RcOutcome::Ok));
        assert_eq!(
            d.block_store().block_snapshots(new_id).unwrap().into_iter()
                .filter(|b| b.kind == BlockKind::Text).map(|b| b.content).collect::<Vec<_>>(),
            vec!["the real script".to_string()],
            "the .kai script runs and the data file is ignored"
        );
    }

    #[tokio::test]
    async fn rc_script_failure_inserts_error_block_continues() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        // S00 returns non-zero; S10 is benign.
        install_rc_script_file(&d, "/config/rc/test/create/S00-fail.kai", "exit 17").await;
        install_rc_script_file(&d, "/config/rc/test/create/S10-after.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'ran-after-failure'"#).await;

        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-mixed", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-mixed");
        let kinds = block_kinds_in(&d, new_id);
        let contents = block_contents_in(&d, new_id);

        assert!(
            kinds.contains(&kaijutsu_types::BlockKind::Error),
            "S00 failure must produce Error block, got kinds: {kinds:?}"
        );
        assert!(
            contents.iter().any(|c| c.contains("ran-after-failure")),
            "S10 must run after S00 fails, got contents: {contents:?}"
        );
        // Sanity: the error content should mention the failing path.
        assert!(
            contents
                .iter()
                .any(|c| c.contains("/config/rc/test/create/S00-fail.kai")),
            "error block should reference rc path, got: {contents:?}"
        );
    }

    #[tokio::test]
    async fn rc_attach_fires_scripts_on_target() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        // The script authors its instruction on the target.
        install_rc_script_file(&d, "/config/rc/test/attach/S00-banner.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'attach-banner-content'"#).await;

        let principal = PrincipalId::new();
        let target = register_context(&d, Some("attach-target"), None, principal);
        set_context_type(&d, target, "test");

        let caller = caller_with_context(target);
        let admission = admit(&d, target);
        let res = crate::rc::run(
            &d,
            test_invocation("attach", &admission),
            &caller,
        )
            .await;
        assert!(res.is_ok(), "attach lifecycle should succeed, got: {res:?}");

        let contents = block_contents_in(&d, target);
        assert!(
            contents.iter().any(|c| c.contains("attach-banner-content")),
            "attach script must author its instruction; got: {contents:?}"
        );
    }

    #[tokio::test]
    async fn admitted_lifecycle_finishes_its_snapshot_after_archive() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let target = register_context(&d, Some("admitted-then-archived"), None, principal);
        set_context_type(&d, target, "archive-race");
        for (order, marker) in [("S00", "first"), ("S10", "second")] {
            install_rc_script_file(&d,
                &format!("/config/rc/archive-race/tick/{order}-marker.kai"),
                &format!("kj block create --role system --kind text --content '{marker}'"),
            ).await;
        }
        let admission = admit(&d, target);
        d.kernel_db().lock().archive_context(target).unwrap();

        crate::rc::run(&d, test_invocation("tick", &admission), &caller_with_context(target))
            .await.expect("accepted lifecycle survives later archive");

        let contents = block_contents_in(&d, target);
        assert!(contents.iter().any(|content| content == "first"), "{contents:?}");
        assert!(contents.iter().any(|content| content == "second"), "{contents:?}");
        let run = find_run_for_context(&d, target, "tick").expect("durable lifecycle run");
        assert_eq!(run.script_count, Some(2));
        assert_eq!(run.outcome, Some(RcOutcome::Ok));
    }

    #[tokio::test]
    async fn rc_recursion_guard_caps_depth() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-noop.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'would-run'"#).await;
        let mut caller = console_caller(&d);
        caller.rc_depth = MAX_RC_DEPTH; // simulate already-deep invocation

        // Construct a fresh context manually via the dispatch path.
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-recur", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok());
        let new_id = lookup_context_id(&d, "ctx-recur");
        let kinds = block_kinds_in(&d, new_id);
        // Recursion guard fires: error block, no instruction block.
        assert!(
            kinds.contains(&kaijutsu_types::BlockKind::Error),
            "guard should insert Error block, got: {kinds:?}"
        );
        let contents = block_contents_in(&d, new_id);
        assert!(
            !contents.iter().any(|c| c.contains("would-run")),
            "guarded run must not insert an instruction block, got: {contents:?}"
        );
    }

    /// Set a registered context's type to `t` so rc dispatch finds scripts
    /// under `/config/rc/<t>/...`. `register_context` defaults to "default".
    fn set_context_type(d: &KjDispatcher, ctx: ContextId, t: &str) {
        let db = d.kernel_db().lock();
        db.update_context_type(ctx, t).expect("update_context_type");
    }

    /// Count blocks whose content contains `needle`.
    fn count_blocks_containing(d: &KjDispatcher, ctx: ContextId, needle: &str) -> usize {
        block_contents_in(d, ctx)
            .iter()
            .filter(|c| c.contains(needle))
            .count()
    }

    #[tokio::test]
    async fn rc_drift_pull_inserts_drift_then_runs_script() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        // .kai script asserts overlay vars look right for a Pull drift.
        // No `kj` calls — so set_self_arc is unnecessary.
        install_rc_script_file(&d, "/config/rc/test/drift/S00-introspect.kai", r#"
[[ -n "$KJ_VERB" ]] || exit 1
[[ -n "$KJ_CONTEXT" ]] || exit 2
[[ -n "$KJ_DRIFT_INFO" ]] || exit 3
case "$KJ_VERB" in
  drift) ;;
  *) exit 4 ;;
esac
case "$KJ_DRIFT_INFO" in
  *'"kind":"pull"'*) ;;
  *) exit 5 ;;
esac
"#).await;

        let principal = PrincipalId::new();
        let dst = register_context(&d, Some("dst"), None, principal);
        set_context_type(&d, dst, "test");
        let src = register_context(&d, Some("src"), None, principal);

        let caller = caller_with_context(dst);
        let admission = admit(&d, dst);
        let res = crate::rc::run(
            &d,
            crate::rc::RcInvocation {
                drift: Some(DriftInfo {
                    kind: DriftKind::Pull,
                    source_ctx: src,
                    target_ctx: dst,
                    source_model: Some("claude-opus-4-7".into()),
                }),
                ..test_invocation("drift", &admission)
            },
            &caller,
        )
            .await;
        assert!(res.is_ok(), "drift rc errored: {res:?}");

        let kinds = block_kinds_in(&d, dst);
        assert!(
            !kinds.contains(&BlockKind::Error),
            ".kai overlay-var assertions failed; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, dst),
        );
    }

    #[tokio::test]
    async fn rc_drift_merge_runs_with_target_overlay() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/drift/S00-introspect.kai", r#"
[[ -n "$KJ_VERB" ]] || exit 1
case "$KJ_VERB" in
  drift) ;;
  *) exit 2 ;;
esac
case "$KJ_DRIFT_INFO" in
  *'"kind":"merge"'*) ;;
  *) exit 3 ;;
esac
"#).await;

        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        set_context_type(&d, parent, "test");
        let child = register_context(&d, Some("child"), Some(parent), principal);

        let caller = caller_with_context(child);
        let admission = admit(&d, parent);
        let res = crate::rc::run(
            &d,
            crate::rc::RcInvocation {
                drift: Some(DriftInfo {
                    kind: DriftKind::Merge,
                    source_ctx: child,
                    target_ctx: parent,
                    source_model: None,
                }),
                ..test_invocation("drift", &admission)
            },
            &caller,
        )
            .await;
        assert!(res.is_ok(), "drift rc errored: {res:?}");

        let kinds = block_kinds_in(&d, parent);
        assert!(
            !kinds.contains(&BlockKind::Error),
            "merge .kai assertions failed; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, parent),
        );
    }

    #[tokio::test]
    async fn rc_drift_flush_fires_per_item() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/drift/S00-marker.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'DRIFT-MARKER'"#).await;

        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        set_context_type(&d, dst, "test");
        // Flush requires a BlockStore document for the destination.
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let caller = caller_with_context(src);
        for content in ["one", "two", "three"] {
            let r = d
                .dispatch(
                    &argv(&["drift", "push", "--stage", "dst", content]),
                    &caller,
                )
                .await;
            assert!(r.is_ok(), "push '{content}' failed: {}", r.message());
        }

        let r = d.dispatch(&argv(&["drift", "flush"]), &caller).await;
        assert!(r.is_ok(), "flush failed: {}", r.message());
        assert!(
            r.message().contains("flushed 3 drift"),
            "expected all 3 flushed, got: {}",
            r.message()
        );

        let marker_count = count_blocks_containing(&d, dst, "DRIFT-MARKER");
        assert_eq!(
            marker_count, 3,
            "expected 3 marker blocks, contents: {:?}",
            block_contents_in(&d, dst)
        );
    }

    #[tokio::test]
    async fn rc_drift_script_failure_inserts_error_continues_flush() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/drift/S00-fail.kai", "exit 17").await;
        install_rc_script_file(&d, "/config/rc/test/drift/S10-after.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'AFTER-MARKER'"#).await;

        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        set_context_type(&d, dst, "test");
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let caller = caller_with_context(src);
        for content in ["one", "two"] {
            d.dispatch(&argv(&["drift", "push", "--stage", "dst", content]), &caller)
                .await;
        }

        let r = d.dispatch(&argv(&["drift", "flush"]), &caller).await;
        assert!(r.is_ok(), "flush failed: {}", r.message());
        assert!(
            r.message().contains("flushed 2 drift"),
            "expected both items reported injected (drift block landed; rc \
             failure does not block delivery), got: {}",
            r.message()
        );

        let kinds = block_kinds_in(&d, dst);
        let error_count = kinds
            .iter()
            .filter(|k| **k == BlockKind::Error)
            .count();
        assert_eq!(error_count, 2, "expected 1 Error per item; kinds: {kinds:?}");
        let after_count = count_blocks_containing(&d, dst, "AFTER-MARKER");
        assert_eq!(
            after_count, 2,
            "S10 must run after S00 fails per-item; contents: {:?}",
            block_contents_in(&d, dst)
        );
    }

    #[tokio::test]
    async fn rc_drift_compact_fork_does_not_double_fire() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/fork/S00-fork.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'FORK-MARKER'"#).await;
        install_rc_script_file(&d, "/config/rc/test/drift/S00-drift.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'DRIFT-MARKER'"#).await;

        let caller = console_caller(&d);
        let r = d
            .dispatch(
                &argv(&["context", "create", "parent", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create parent failed: {}", r.message());
        let parent_id = lookup_context_id(&d, "parent");

        // `kj context create` writes the KernelDb context+document but
        // doesn't seed a BlockStore document unless the rc create
        // lifecycle inserted blocks. With no `create` script for
        // `test`, the BlockStore doc isn't created — seed it explicitly
        // so insert_block_as has somewhere to land.
        d.block_store()
            .create_document(parent_id, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Insert a block so --compact has something to distill (otherwise
        // the distillation path errors out before reaching rc).
        d.block_store()
            .insert_block_as(
                parent_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "seed content for compact-fork distillation".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::system()),
            )
            .unwrap();

        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate. This
        // test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
        let r = d
            .dispatch(
                &argv(&["fork", "--name", "child", "--compact"]),
                &fork_caller,
            )
            .await;
        // Compact may fail in tests if no LLM is wired; if so, the rc
        // call doesn't fire and the test premise is moot. Skip cleanly.
        if !r.is_ok() {
            eprintln!(
                "skipping compact-fork rc check — fork --compact unavailable in test: {}",
                r.message()
            );
            return;
        }

        let child_id = lookup_context_id(&d, "child");
        let fork_marker = count_blocks_containing(&d, child_id, "FORK-MARKER");
        let drift_marker = count_blocks_containing(&d, child_id, "DRIFT-MARKER");
        assert!(
            fork_marker >= 1,
            "child should have FORK-MARKER from fork rc, got contents: {:?}",
            block_contents_in(&d, child_id)
        );
        assert_eq!(
            drift_marker, 0,
            "compact-fork must NOT fire drift rc on the new context; got {drift_marker} marker(s) in: {:?}",
            block_contents_in(&d, child_id)
        );
    }

    #[tokio::test]
    async fn rc_fork_exposes_parent_block_count() {
        // Verifies KJ_PARENT_BLOCK_COUNT carries the parent's
        // BlockStore size at fork time — the number rc-on-fork scripts
        // need to compute the MessageIndex(N - 1) fork-point cache
        // breakpoint. Captured from the parent's BlockStore (not the
        // child's) because the child's count already includes the
        // fork-marker block by the time this rc hook fires.
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/fork/S00-assert-parent-count.kai", // Three explicit assertions, each with a distinct exit code
            // so a regression points at the right one:
            //   exit 1 — env var missing
            //   exit 2 — env var not a positive integer
            //   exit 3 — env var doesn't match the seeded count of 3
            r#"
[[ -n "$KJ_PARENT_BLOCK_COUNT" ]] || exit 1
case "$KJ_PARENT_BLOCK_COUNT" in
  ''|*[!0-9]*) exit 2 ;;
esac
case "$KJ_PARENT_BLOCK_COUNT" in
  3) ;;
  *) exit 3 ;;
esac
"#).await;

        // Parent context, typed "test" so the fork hook above fires.
        let caller = console_caller(&d);
        let r = d
            .dispatch(
                &argv(&["context", "create", "parent", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create parent failed: {}", r.message());
        let parent_id = lookup_context_id(&d, "parent");

        // `kj context create` writes KernelDb but doesn't seed the
        // BlockStore unless an rc-on-create script does. Seed it
        // explicitly so we can insert exactly the count we want.
        d.block_store()
            .create_document(parent_id, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Seed exactly 3 blocks. The rc script's case match pins this.
        for content in ["a", "b", "c"] {
            d.block_store()
                .insert_block_as(
                    parent_id,
                    None,
                    None,
                    Role::User,
                    BlockKind::Text,
                    content.to_string(),
                    Status::Done,
                    ContentType::Plain,
                    Some(PrincipalId::system()),
                )
                .unwrap();
        }
        assert_eq!(
            d.block_store()
                .block_snapshots(parent_id)
                .unwrap()
                .len(),
            3,
            "parent must have exactly 3 blocks before fork — drives the rc script's case match"
        );

        // Fork. Parent's blocks copy into the child, then the fork
        // marker injects (taking the child's count to >3), then
        // rc-on-fork runs and reads KJ_PARENT_BLOCK_COUNT.
        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate. This
        // test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
        let r = d
            .dispatch(&argv(&["fork", "--name", "child"]), &fork_caller)
            .await;
        assert!(r.is_ok(), "fork failed: {}", r.message());

        let child_id = lookup_context_id(&d, "child");
        let kinds = block_kinds_in(&d, child_id);
        assert!(
            !kinds.contains(&BlockKind::Error),
            "rc-on-fork assertions tripped; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, child_id)
        );
    }

    #[tokio::test]
    async fn rc_create_omits_parent_block_count() {
        // KJ_PARENT_BLOCK_COUNT is fork-only — rc-on-create has no
        // parent, so the var must be absent (not "0", not "").
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-no-parent-count.kai", // Exit 99 if the var is set to anything. Empty/unset env
            // vars in kaish expand to empty string under `$VAR`, so
            // `[[ -z … ]]` catches both.
            r#"
[[ -z "$KJ_PARENT_BLOCK_COUNT" ]] || exit 99
"#).await;

        let caller = console_caller(&d);
        let r = d
            .dispatch(
                &argv(&["context", "create", "solo", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create failed: {}", r.message());

        let id = lookup_context_id(&d, "solo");
        let kinds = block_kinds_in(&d, id);
        assert!(
            !kinds.contains(&BlockKind::Error),
            "create rc must not see KJ_PARENT_BLOCK_COUNT; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, id)
        );
    }

    #[tokio::test]
    async fn rc_fork_does_not_trigger_create_scripts() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-only-create.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'CREATE-MARKER'"#).await;
        install_rc_script_file(&d, "/config/rc/test/fork/S00-only-fork.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'FORK-MARKER'"#).await;

        // Step 1: create parent (CREATE-MARKER appears in parent).
        let caller = console_caller(&d);
        d.dispatch(&argv(&["context", "create", "parent", "--type", "test"]), &caller)
            .await;
        let parent_id = lookup_context_id(&d, "parent");
        let parent_contents = block_contents_in(&d, parent_id);
        assert!(
            parent_contents.iter().any(|c| c.contains("CREATE-MARKER")),
            "parent should have CREATE-MARKER, got: {parent_contents:?}"
        );

        // Step 2: fork the parent. Use a caller bound to the parent so
        // fork resolves "."  to the parent context.
        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate. This
        // test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
        let fork_result = d
            .dispatch(&argv(&["fork", "--name", "child"]), &fork_caller)
            .await;
        assert!(fork_result.is_ok(), "fork failed: {}", fork_result.message());

        let child_id = lookup_context_id(&d, "child");
        let child_contents = block_contents_in(&d, child_id);
        assert!(
            child_contents.iter().any(|c| c.contains("FORK-MARKER")),
            "child should have FORK-MARKER, got: {child_contents:?}"
        );
        // The forked document inherits parent blocks (which include
        // CREATE-MARKER from the parent's create lifecycle), so we
        // can't assert CREATE-MARKER is absent. The verb-isolation
        // guarantee is: no NEW CREATE-MARKER is inserted at fork time.
        // Count occurrences instead.
        let create_marker_count = child_contents
            .iter()
            .filter(|c| c.contains("CREATE-MARKER"))
            .count();
        assert_eq!(
            create_marker_count, 1,
            "fork must not run create-side scripts (would duplicate marker), got: {child_contents:?}"
        );
    }

    /// Cancellation keeps effects an earlier script committed, but waits for
    /// the active interpreter to clean up and never starts a later script.
    #[tokio::test]
    async fn cancelling_an_rc_lifecycle_stops_later_scripts_after_cleanup() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let context = register_context(&d, Some("cancel-lifecycle"), None, PrincipalId::new());
        set_context_type(&d, context, "cancel");
        install_rc_script_file(
            &d,
            "/config/rc/cancel/create/S00-first.kai",
            "echo retained-active-output; kj block create --role system --kind notification --content retained-before-cancel; sleep 10",
        )
        .await;
        install_rc_script_file(
            &d,
            "/config/rc/cancel/create/S20-later.kai",
            "kj block create --role system --kind notification --content must-not-run",
        )
        .await;

        let admission = admit(&d, context);
        let cancel = tokio_util::sync::CancellationToken::new();
        let invocation = RcInvocation::new("create", &admission, &cancel);
        let caller = caller_with_context(context);
        let mut inspect = tokio::time::interval(std::time::Duration::from_millis(5));
        let run = crate::rc::run(&d, invocation, &caller);
        tokio::pin!(run);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut run => panic!("rc lifecycle ended before cancellation: {result:?}"),
                    _ = inspect.tick() => {
                        if block_contents_in(&d, context).iter().any(|content| content.contains("retained-before-cancel")) {
                            break;
                        }
                    }
                }
            }
        }).await.expect("the active script must commit its marker before cancellation");

        cancel.cancel();
        let result = match tokio::time::timeout(std::time::Duration::from_secs(2), &mut run).await {
            Ok(result) => result,
            Err(_) => {
                panic!("cancellation must await interpreter cleanup instead of waiting for sleep")
            }
        };
        let error = result.expect_err("cancelled lifecycle must return a cancellation error");
        assert!(error.contains("cancel"), "{error}");
        assert!(
            block_contents_in(&d, context)
                .iter()
                .any(|content| content.contains("retained-active-output")),
            "cancellation diagnostic must retain output from the active script"
        );
        assert!(
            !block_contents_in(&d, context).iter().any(|content| content.contains("must-not-run")),
            "cancellation must stop later scripts"
        );
        let run = find_run_for_context(&d, context, "create").expect("durable run row");
        assert_eq!(run.outcome, Some(RcOutcome::Failed));
    }

    /// A lifecycle re-entering rc through `kj` remains part of the same owner.
    /// Cancellation must reach the child interpreter and await its cleanup,
    /// while preserving the child context and effects committed before it.
    #[test]
    fn cancelling_an_rc_lifecycle_cancels_nested_create_after_cleanup() {
        crate::spawn_kaish_thread("rc-test-thread", || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(cancelling_an_rc_lifecycle_cancels_nested_create_after_cleanup_body());
        })
        .unwrap()
        .join()
        .unwrap();
    }

    async fn cancelling_an_rc_lifecycle_cancels_nested_create_after_cleanup_body() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let outer = register_context(&d, Some("nested-cancel-outer"), None, PrincipalId::new());
        set_context_type(&d, outer, "nested-cancel-outer");
        install_rc_script_file(
            &d,
            "/config/rc/nested-cancel-outer/create/S00-create-child.kai",
            "kj context create nested-cancel-child --type nested-cancel-child",
        )
        .await;
        install_rc_script_file(
            &d,
            "/config/rc/nested-cancel-child/create/S00-block.kai",
            "echo retained-nested-output; kj block create --role system --kind notification --content nested-before-cancel; sleep 10",
        )
        .await;
        install_rc_script_file(
            &d,
            "/config/rc/nested-cancel-child/create/S20-later.kai",
            "kj block create --role system --kind notification --content nested-must-not-run",
        )
        .await;

        let admission = admit(&d, outer);
        let cancel = tokio_util::sync::CancellationToken::new();
        let invocation = RcInvocation::new("create", &admission, &cancel);
        let caller = caller_with_context(outer);
        let mut inspect = tokio::time::interval(std::time::Duration::from_millis(5));
        let run = crate::rc::run(&d, invocation, &caller);
        tokio::pin!(run);
        let child = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut run => panic!("outer lifecycle ended before nested cancellation: {result:?}"),
                    _ = inspect.tick() => {
                        let child = d.kernel_db().lock()
                            .find_context_by_label("nested-cancel-child").unwrap()
                            .map(|row| row.context_id);
                        if let Some(child) = child
                            && block_contents_in(&d, child).iter()
                                .any(|content| content.contains("nested-before-cancel"))
                        {
                            break child;
                        }
                    }
                }
            }
        }).await.expect("the nested lifecycle must commit its marker before cancellation");

        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), &mut run)
            .await
            .expect("nested cancellation must await cleanup without waiting for sleep");
        let error = result.expect_err("the cancelled outer lifecycle must report cancellation");
        assert!(error.contains("cancel"), "{error}");
        assert!(
            d.kernel_db().lock().get_context(child).unwrap().is_some(),
            "the committed child context must survive cancellation"
        );
        let contents = block_contents_in(&d, child);
        assert!(
            contents.iter().any(|content| content.contains("retained-nested-output")),
            "the nested cancellation diagnostic must retain active output: {contents:?}"
        );
        assert!(
            !contents.iter().any(|content| content.contains("nested-must-not-run")),
            "nested cancellation must stop later child scripts: {contents:?}"
        );
        let child_run = find_run_for_context(&d, child, "create").expect("nested create run");
        assert_eq!(child_run.outcome, Some(RcOutcome::Failed));
    }

    /// All `.kai` scripts run under the kernel-wide `rc_script_timeout`
    /// (per-script overrides were dropped with the move to files). Pin it
    /// to 200ms, well under the script's 1s sleep, and confirm the runaway
    /// script is killed with an Error block and never completes.
    #[tokio::test]
    async fn rc_kernel_default_timeout_kills_runaway_script() {
        let policy = kaijutsu_types::TimeoutPolicy {
            rc_script_timeout: std::time::Duration::from_millis(200),
            ..Default::default()
        };
        let d = crate::kj::test_helpers::test_dispatcher_with_timeouts(policy).await;

        install_rc_script_file(&d, "/config/rc/test/create/S00-slow.kai", "sleep 1 && echo never-reached")
        .await;

        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-default-kills", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-default-kills");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            kinds.contains(&kaijutsu_types::BlockKind::Error),
            "200ms kernel default must kill 1s sleep; kinds: {kinds:?}"
        );
        let contents = block_contents_in(&d, new_id);
        assert!(
            !contents.iter().any(|c| c.contains("never-reached")),
            "script body must not have completed; contents: {contents:?}"
        );
    }

    // ── Context time awareness ────────────────────────────────────────────
    //
    // These exercise the REAL embedded seed tree (assets/defaults/rc), not
    // synthetic install_script fixtures, because the mechanism under test is
    // the per-type `SXX-datetime.kai` seed *and* its init.d-style symlink
    // composition (`coder/create/S25-datetime.kai` → `lib/create/S25-
    // datetime.kai`, reconstructed as a real host symlink by
    // `ensure_rc_seed_files`). So all tests below use `test_dispatcher_rc()`.

    /// The marker every rc-seeded datetime notification's content starts
    /// with — kept in one place so a wording tweak in the seed scripts
    /// doesn't scatter edits across every test.
    const DATETIME_MARKER: &str = "Current date/time: ";

    fn notification_blocks_with_marker(
        dispatcher: &KjDispatcher,
        ctx: ContextId,
    ) -> Vec<kaijutsu_types::BlockSnapshot> {
        dispatcher
            .block_store()
            .block_snapshots(ctx)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| b.kind == BlockKind::Notification && b.content.contains(DATETIME_MARKER))
            .collect()
    }

    /// Parse a `YYYY-MM-DD` prefix (ASCII digits only — the shape `date
    /// '+%Y-%m-%d'` always produces).
    fn parse_iso_date(s: &str) -> Option<(i64, i64, i64)> {
        if s.len() < 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
            return None;
        }
        let y = s.get(0..4)?.parse().ok()?;
        let m = s.get(5..7)?.parse().ok()?;
        let d = s.get(8..10)?.parse().ok()?;
        Some((y, m, d))
    }

    /// Howard Hinnant's `days_from_civil`: days since the 1970-01-01 epoch
    /// for a proleptic-Gregorian `(y, m, d)`. Pure integer math — just
    /// enough to sanity-check the rc-seeded date is "today" without pulling
    /// in a date crate for one test.
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    fn days_since_epoch_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_secs() as i64
            / 86400
    }

    /// coder create seeds exactly one datetime `Notification` block, it
    /// carries today's date, it never leaks into the cached system-prompt
    /// sections, and it actually hydrates into the conversation the model
    /// sees. This is the full mechanism proof; the sibling tests below only
    /// check the per-type policy matrix (which types get one, which don't).
    #[tokio::test]
    async fn coder_create_seeds_model_visible_datetime_notification() {
        // `kj` is only registered inside rc `.kai` scripts once the
        // dispatcher's self-Arc is wired (see `rc_kai_can_call_kj` above) —
        // and the coder stance/binding/datetime scripts all call `kj`.
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let caller = console_caller(&d);
        let r = d
            .dispatch(
                &argv(&["context", "create", "c1", "--type", "coder"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create failed: {}", r.message());
        let ctx = lookup_context_id(&d, "c1");

        let notes = notification_blocks_with_marker(&d, ctx);
        assert_eq!(
            notes.len(),
            1,
            "expected exactly one datetime notification at create, got: {:?}",
            block_contents_in(&d, ctx)
        );
        let note = &notes[0];
        assert_eq!(
            note.role,
            Role::System,
            "datetime note should be system-authored"
        );

        // The date it carries must be "today" — within a day, to absorb the
        // gap between the script's local-TZ render and this UTC reference.
        let date_str = note
            .content
            .strip_prefix(DATETIME_MARKER)
            .and_then(|rest| rest.get(0..10))
            .unwrap_or_else(|| panic!("marker prefix + date substring in {:?}", note.content));
        let (y, m, day) = parse_iso_date(date_str)
            .unwrap_or_else(|| panic!("parseable ISO date in {date_str:?}"));
        let got_days = days_from_civil(y, m, day);
        assert!(
            (got_days - days_since_epoch_now()).abs() <= 1,
            "seeded date {date_str} is not within a day of now"
        );

        // Must NOT land in the cached system prompt (the cache-placement
        // rule) — a BlockKind::Text system block would fold in here and
        // invalidate the system cache breakpoint every time the date rolls.
        let blocks = d.block_store().block_snapshots(ctx).unwrap();
        let sections = crate::extract_system_prompt_sections(&blocks);
        assert!(
            sections.iter().all(|s| !s.contains(DATETIME_MARKER)),
            "datetime note leaked into the cached system-prompt sections: {sections:?}"
        );

        // Must actually reach the model as an appended conversation message.
        let messages = crate::hydrate_from_blocks(&blocks);
        assert!(
            messages.iter().any(|m| matches!(
                &m.content,
                crate::llm::MessageContent::Text(t) if t.contains(DATETIME_MARKER)
            )),
            "datetime note did not hydrate into the conversation"
        );
    }

    /// The per-type policy matrix, create side: coder/director/mcp/default
    /// ("time-aware" types) each get exactly one seeded datetime note.
    #[tokio::test]
    async fn create_seeds_datetime_notification_for_time_aware_types() {
        for context_type in ["coder", "director", "mcp", "default"] {
            let d = std::sync::Arc::new(test_dispatcher_rc().await);
            d.set_self_arc();
            let caller = console_caller(&d);
            let label = format!("c-{context_type}");
            let r = d
                .dispatch(
                    &argv(&["context", "create", &label, "--type", context_type]),
                    &caller,
                )
                .await;
            assert!(r.is_ok(), "{context_type} create failed: {}", r.message());
            let ctx = lookup_context_id(&d, &label);
            let notes = notification_blocks_with_marker(&d, ctx);
            assert_eq!(
                notes.len(),
                1,
                "{context_type}: expected exactly one datetime notification, got: {:?}",
                block_contents_in(&d, ctx)
            );
        }
    }

    /// The other half of the matrix: musician and toolie must NOT get a
    /// wall-clock note. Musical time (`$PHRASE`/ticks/the track clock) is
    /// the musician's only time base; a wall-clock drip is pure cache/
    /// attention pollution for both narrow roles.
    #[tokio::test]
    async fn create_seeds_no_datetime_notification_for_clockless_types() {
        for context_type in ["musician", "toolie"] {
            let d = std::sync::Arc::new(test_dispatcher_rc().await);
            d.set_self_arc();
            let caller = console_caller(&d);
            let label = format!("c-{context_type}");
            let r = d
                .dispatch(
                    &argv(&["context", "create", &label, "--type", context_type]),
                    &caller,
                )
                .await;
            assert!(r.is_ok(), "{context_type} create failed: {}", r.message());
            let ctx = lookup_context_id(&d, &label);
            let notes = notification_blocks_with_marker(&d, ctx);
            assert!(
                notes.is_empty(),
                "{context_type}: must stay clock-free, got: {:?}",
                block_contents_in(&d, ctx)
            );
        }
    }

    /// Fork re-seeds: the child gets its own fresh datetime note on top of
    /// whatever it inherited from the parent (the parent's create-time note
    /// copies over too — fork copies the whole log — so the count grows by
    /// exactly one, not to exactly one).
    #[tokio::test]
    async fn coder_fork_reseeds_datetime_notification() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let caller = console_caller(&d);
        let r = d
            .dispatch(
                &argv(&["context", "create", "parent", "--type", "coder"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create failed: {}", r.message());
        let parent_id = lookup_context_id(&d, "parent");
        let before = notification_blocks_with_marker(&d, parent_id).len();
        assert_eq!(
            before, 1,
            "parent should carry its create-time datetime note"
        );

        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate.
        // This test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
        let fr = d
            .dispatch(&argv(&["fork", "--name", "child"]), &fork_caller)
            .await;
        assert!(fr.is_ok(), "fork failed: {}", fr.message());
        let child_id = lookup_context_id(&d, "child");
        let after = notification_blocks_with_marker(&d, child_id).len();
        assert_eq!(
            after,
            before + 1,
            "fork must re-seed exactly one fresh datetime note on top of the copied parent one, got: {:?}",
            block_contents_in(&d, child_id)
        );
    }

    // ── The rc run log (approval_ledger::rc_runs) ──────────────────────

    /// Find the run-log row for `(ctx, verb)`. Every test in this section
    /// fires exactly one rc verb per context, so "the one with this verb"
    /// is unambiguous.
    fn find_run_for_context(
        d: &KjDispatcher,
        ctx: ContextId,
        verb: &str,
    ) -> Option<approval_ledger::types::RcRunRow> {
        let db = d.kernel_db().lock();
        let ctx_bytes = ctx.as_bytes().to_vec();
        approval_ledger::rc_runs::list_runs(db.conn_for_ledger())
            .unwrap()
            .into_iter()
            .find(|r| r.context_id == ctx_bytes && r.verb == verb)
    }

    /// The key test this whole slice exists for: a lifecycle run that FAILS
    /// still leaves a FINISHED row with a `Failed` outcome. This is exactly
    /// the incident `approval_ledger::rc_runs`'s module doc names — a
    /// startup rc sweep that silently never ran, with nothing recording
    /// that — except here the failure is loud (an Error block already
    /// exists for it), and the run log's job is to make the *absence* of a
    /// run just as visible as a loud one.
    #[tokio::test]
    async fn a_failing_rc_script_leaves_a_finished_run_with_failed_outcome() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-fail.kai", "exit 9")
        .await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-run-fail", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-run-fail");

        let run = find_run_for_context(&d, new_id, "create")
            .expect("a run row must exist for this (context, verb)");
        assert!(
            run.finished_at.is_some(),
            "a failed run must still be FINISHED, not left looking like it's still \
             running: {run:?}"
        );
        assert_eq!(
            run.outcome,
            Some(approval_ledger::types::RcOutcome::Failed),
            "a script that exited nonzero must fail the run: {run:?}"
        );
    }

    /// The non-vacuity companion: a clean lifecycle records started AND
    /// finished, with an `Ok` outcome — proving the assertion above pins a
    /// real failure signal, not an always-`None`/always-absent row.
    #[tokio::test]
    async fn a_successful_rc_lifecycle_records_a_finished_ok_run() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-noop.kai", "true")
        .await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-run-ok", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-run-ok");

        let run = find_run_for_context(&d, new_id, "create")
            .expect("a run row must exist for this (context, verb)");
        assert!(run.started_at > 0);
        assert!(run.finished_at.is_some(), "a clean run must still be finished: {run:?}");
        assert_eq!(run.outcome, Some(approval_ledger::types::RcOutcome::Ok));
    }

    /// Per-script rows land in `rc_run_scripts`, in order, each with its
    /// real exit code and a content-addressed body hash — the detail `kj
    /// ledger runs <run-id>` reads.
    #[tokio::test]
    async fn rc_run_records_per_script_rows_in_order() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-first.kai", "true")
        .await;
        install_rc_script_file(&d, "/config/rc/test/create/S10-second.kai", "exit 5")
        .await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-run-scripts", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-run-scripts");

        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        let scripts = {
            let db = d.kernel_db().lock();
            approval_ledger::rc_runs::list_run_scripts(db.conn_for_ledger(), &run.run_id).unwrap()
        };
        assert_eq!(scripts.len(), 2, "both scripts must be recorded: {scripts:?}");
        assert!(scripts[0].path.ends_with("S00-first.kai"), "{:?}", scripts[0]);
        assert_eq!(scripts[0].exit_code, Some(0));
        assert!(scripts[1].path.ends_with("S10-second.kai"), "{:?}", scripts[1]);
        assert_eq!(scripts[1].exit_code, Some(5));
        for s in &scripts {
            let finished = s.finished_at.expect("finished_at recorded");
            assert!(
                s.started_at <= finished,
                "started_at must not be after finished_at: {s:?}"
            );
        }
        assert_eq!(
            run.outcome,
            Some(approval_ledger::types::RcOutcome::Failed),
            "one failing script among several fails the whole run (SysV init.d \
             semantics: every script still runs)"
        );
    }

    /// A run that hits the recursion guard runs NO scripts and still
    /// finishes as `Failed` — that guard is itself a failure, not a
    /// no-op, and must not leave a dangling unfinished row.
    #[tokio::test]
    async fn a_recursion_guarded_run_still_finishes_as_failed() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/test/create/S00-noop.kai", r#"kj block create --role system --kind text --content-type text/markdown --content 'would-run'"#)
        .await;
        let mut caller = console_caller(&d);
        caller.rc_depth = MAX_RC_DEPTH;

        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-run-guarded", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok());
        let new_id = lookup_context_id(&d, "ctx-run-guarded");

        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert!(run.finished_at.is_some());
        assert_eq!(run.outcome, Some(approval_ledger::types::RcOutcome::Failed));
    }

    /// A verb whose directory EXISTS but has no `.kai` scripts in it
    /// still leaves a finished `Ok` run — the empty case is legitimate, not
    /// a gap in the log. Distinct from `rc_no_scripts_for_type_is_noop`,
    /// which covers a type with no rc directory at all: this one installs a
    /// non-script file so the directory is real and non-empty, exercising
    /// "readdir succeeds, nothing matches `.kai`" rather than "readdir
    /// reports the directory missing."
    #[tokio::test]
    async fn a_verb_with_no_scripts_still_finishes_as_ok() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        install_rc_script_file(
            &d,
            "/config/rc/emptytype/create/README.txt",
            "not an rc script — just here to make the directory exist",
        )
        .await;
        let caller = console_caller(&d);
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-run-empty", "--type", "emptytype"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-run-empty");

        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        assert!(run.finished_at.is_some());
        assert_eq!(run.outcome, Some(approval_ledger::types::RcOutcome::Ok));
    }

    /// Scripts run in lexical filename order regardless of creation order,
    /// with a same-prefix tie broken by name. Files are created in reverse
    /// so directory or insertion order cannot satisfy the assertion.
    #[tokio::test]
    async fn rc_scripts_run_in_lexical_order_with_name_tiebreak() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        for name in ["S10-b", "S10-a", "S00-z"] {
            install_rc_script_file(
                &d,
                &format!("/config/rc/test/create/{name}.kai"),
                &format!(
                    "kj block create --role system --kind text --content-type text/markdown --content 'order-{name}'"
                ),
            )
            .await;
        }
        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-order", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-order");

        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        let scripts = {
            let db = d.kernel_db().lock();
            approval_ledger::rc_runs::list_run_scripts(db.conn_for_ledger(), &run.run_id).unwrap()
        };
        let paths: Vec<&str> = scripts.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/config/rc/test/create/S00-z.kai",
                "/config/rc/test/create/S10-a.kai",
                "/config/rc/test/create/S10-b.kai",
            ],
            "run rows must be in lexical order"
        );

        let authored: Vec<String> = block_contents_in(&d, new_id)
            .into_iter()
            .filter(|c| c.starts_with("order-"))
            .collect();
        assert_eq!(
            authored,
            ["order-S00-z", "order-S10-a", "order-S10-b"],
            "authored block order must follow script order"
        );
    }

    /// Source identity is the link path: an init.d-style symlink is recorded,
    /// and named in failure blocks, by the path the rc directory lists, not by
    /// the file it resolves to.
    #[tokio::test]
    async fn rc_symlinked_script_identity_is_the_link_path() {
        use crate::vfs::VfsOps;
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        install_rc_script_file(&d, "/config/rc/lib/create/S00-shared.kai", "exit 3").await;
        d.kernel()
            .vfs()
            .symlink(
                std::path::Path::new("/config/rc/test/create/S00-stance.kai"),
                std::path::Path::new("../../lib/create/S00-shared.kai"),
            )
            .await
            .expect("create rc symlink");

        let caller = console_caller(&d);
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-link-id", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-link-id");

        let link = "/config/rc/test/create/S00-stance.kai";
        let run = find_run_for_context(&d, new_id, "create").expect("run row");
        let scripts = {
            let db = d.kernel_db().lock();
            approval_ledger::rc_runs::list_run_scripts(db.conn_for_ledger(), &run.run_id).unwrap()
        };
        assert_eq!(scripts.len(), 1, "{scripts:?}");
        assert_eq!(scripts[0].path, link, "run row must record the link path");
        assert_eq!(scripts[0].exit_code, Some(3));

        let errors: Vec<String> = d
            .block_store()
            .block_snapshots(new_id)
            .expect("read blocks")
            .into_iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Error)
            .map(|b| b.content)
            .collect();
        assert!(
            errors.iter().any(|c| c.contains(link)),
            "Error block must name the link path: {errors:?}"
        );
        assert!(
            errors.iter().all(|c| !c.contains("S00-shared.kai")),
            "Error block must not name the resolved target: {errors:?}"
        );
    }

    /// Verbs whose only caller is the beat scheduler in `kaijutsu-server`
    /// (`BeatScheduler::fire_tick` / `fire_rotate`); the kernel crate cannot
    /// reach it. Its tests live in `kaijutsu-server/src/beat.rs`.
    const SERVER_FIRED_VERBS: &[&str] = &[VERB_TICK, VERB_ROTATE];

    fn run_rows_for_verb(d: &KjDispatcher, verb: &str) -> Vec<approval_ledger::types::RcRunRow> {
        let db = d.kernel_db().lock();
        approval_ledger::rc_runs::list_runs(db.conn_for_ledger())
            .unwrap()
            .into_iter()
            .filter(|r| r.verb == verb)
            .collect()
    }

    /// Fire one verb through the caller users reach, with a marker script
    /// installed. Returns the dispatcher and the context the run belongs to.
    async fn fire_verb_through_its_caller(verb: &str) -> (std::sync::Arc<KjDispatcher>, ContextId) {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        d.kernel().broker().set_kj_dispatcher(&d).await;
        install_rc_script_file(&d, &format!("/config/rc/test/{verb}/S00-noop.kai"), "true").await;
        let principal = PrincipalId::new();
        let ctx = match verb {
            VERB_CREATE => {
                let r = d
                    .dispatch(
                        &argv(&["context", "create", "wired-create", "--type", "test"]),
                        &console_caller(&d),
                    )
                    .await;
                assert!(r.is_ok(), "create failed: {}", r.message());
                lookup_context_id(&d, "wired-create")
            }
            VERB_FORK => {
                // Fork runs the child's `fork` scripts; the parent's type is
                // irrelevant, so build it without a create lifecycle.
                let parent = register_context(&d, Some("wired-parent"), None, principal);
                set_context_type(&d, parent, "test");
                d.block_store()
                    .create_document(parent, crate::DocumentKind::Conversation, None)
                    .unwrap();
                let caller = KjCaller { privileged: true, ..caller_with_context(parent) };
                let r = d.dispatch(&argv(&["fork", "--name", "wired-child"]), &caller).await;
                assert!(r.is_ok(), "fork failed: {}", r.message());
                lookup_context_id(&d, "wired-child")
            }
            VERB_ATTACH => {
                let target = register_context(&d, Some("wired-attach"), None, principal);
                set_context_type(&d, target, "test");
                let r = d.dispatch(&argv(&["attach", "wired-attach"]), &console_caller(&d)).await;
                assert!(matches!(r, crate::KjResult::Switch(..)), "attach failed: {}", r.message());
                target
            }
            VERB_DRIFT => {
                let src = register_context(&d, Some("wired-src"), None, principal);
                let dst = register_context(&d, Some("wired-dst"), None, principal);
                set_context_type(&d, dst, "test");
                d.block_store()
                    .create_document(dst, crate::DocumentKind::Conversation, None)
                    .unwrap();
                let caller = caller_with_context(src);
                let r = d
                    .dispatch(&argv(&["drift", "push", "--stage", "wired-dst", "hello"]), &caller)
                    .await;
                assert!(r.is_ok(), "push failed: {}", r.message());
                let r = d.dispatch(&argv(&["drift", "flush"]), &caller).await;
                assert!(r.is_ok(), "flush failed: {}", r.message());
                dst
            }
            VERB_SUBMIT => {
                let ctx = register_context(&d, Some("wired-submit"), None, principal);
                set_context_type(&d, ctx, "test");
                d.block_store()
                    .create_document(ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();
                d.block_store().edit_draft(ctx, principal, 0, "hello", 0).unwrap();
                // No provider is configured, so startup fails after the
                // lifecycle; only the run row is under test.
                let _ = crate::runtime::prompt::submit(
                    d.kernel(),
                    ctx,
                    principal,
                    kaijutsu_types::SessionId::new(),
                    crate::runtime::prompt::PromptSource::Draft { edge: None },
                )
                .await;
                ctx
            }
            other => panic!(
                "RC_VERBS gained '{other}': add its real caller here, or list it in \
                 SERVER_FIRED_VERBS and cover it in kaijutsu-server beat tests"
            ),
        };
        (d, ctx)
    }

    /// Every canonical verb has a caller that reaches `rc::run` and leaves a
    /// run row. Deleting a caller (or renaming its verb string) fails here.
    #[tokio::test]
    async fn every_kernel_fired_verb_is_wired_through_its_caller() {
        for verb in RC_VERBS.iter().filter(|v| !SERVER_FIRED_VERBS.contains(v)) {
            let (d, ctx) = fire_verb_through_its_caller(verb).await;
            let rows = run_rows_for_verb(&d, verb);
            assert_eq!(rows.len(), 1, "verb '{verb}' must leave exactly one run row: {rows:?}");
            assert_eq!(rows[0].context_id, ctx.as_bytes().to_vec(), "verb '{verb}' ran on the wrong context");
            assert!(rows[0].finished_at.is_some(), "verb '{verb}' run must finish: {:?}", rows[0]);
            let db = d.kernel_db().lock();
            let scripts =
                approval_ledger::rc_runs::list_run_scripts(db.conn_for_ledger(), &rows[0].run_id).unwrap();
            assert_eq!(scripts.len(), 1, "verb '{verb}' must run its marker script: {scripts:?}");
            drop(db);
            d.kernel().shutdown_runtime_worker().await.unwrap();
        }
        for verb in SERVER_FIRED_VERBS {
            assert!(RC_VERBS.contains(verb), "SERVER_FIRED_VERBS names a non-canonical verb '{verb}'");
        }
    }

    // ── submit verb (docs/prompts.md, "The submit verb") ─────────────────

    #[test]
    fn submit_info_vars_sets_all_names_with_optional_facts() {
        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let input_block = BlockId::new(ctx, principal, 2);
        let edge_block = BlockId::new(ctx, principal, 1);
        let log_tail = BlockId::new(ctx, principal, 0);
        let info = SubmitInfo {
            input_block,
            edge_block: Some(edge_block),
            edge_shown: Some(42),
            log_tail: Some(log_tail),
            turn_live: true,
        };

        let vars = info.vars();
        assert_eq!(vars.len(), 5, "every name is always set: {vars:?}");
        assert_eq!(vars["KJ_INPUT_BLOCK"], input_block.to_key());
        assert_eq!(vars["KJ_EDGE_BLOCK"], edge_block.to_key());
        assert_eq!(vars["KJ_EDGE_SHOWN"], "42");
        assert_eq!(vars["KJ_LOG_TAIL"], log_tail.to_key());
        assert_eq!(vars["KJ_TURN_LIVE"], "true");
    }

    #[test]
    fn submit_info_vars_sets_all_names_without_optional_facts() {
        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let input_block = BlockId::new(ctx, principal, 0);
        let info = SubmitInfo {
            input_block,
            edge_block: None,
            edge_shown: None,
            log_tail: None,
            turn_live: false,
        };

        let vars = info.vars();
        assert_eq!(vars.len(), 5, "every name is always set: {vars:?}");
        assert_eq!(vars["KJ_INPUT_BLOCK"], input_block.to_key());
        assert_eq!(vars["KJ_EDGE_BLOCK"], "", "empty, not absent, when unset");
        assert_eq!(vars["KJ_EDGE_SHOWN"], "");
        assert_eq!(vars["KJ_LOG_TAIL"], "");
        assert_eq!(vars["KJ_TURN_LIVE"], "false");
    }

    #[test]
    fn submit_is_a_canonical_wired_verb() {
        assert!(RC_VERBS.contains(&VERB_SUBMIT));
        assert!(verb_is_wired("submit"));
    }
