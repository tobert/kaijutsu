//! The two acts that always ask, over the product wire (`docs/character.md`,
//! "Gate"). A person's uncovered statements run without an ask, but a
//! person's `kj context archive` of their console and `kj character set
//! <name> --no-root` raise one. The person answers it, an allow runs the
//! command, and a denial runs nothing.

mod common;
use common::*;

use std::time::Duration;

use kaijutsu_client::KernelHandle;
use kaijutsu_types::ContextId;

/// Poll until `check` holds, or fail. The approval worker runs in the
/// background, so a timeout is the bug.
async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

/// Run `kj <argv>` in `context` as the connection, and return its exit code
/// and output; a refusal on the wire reads as exit code -1.
async fn kj(kernel: &KernelHandle, context: ContextId, argv: &[&str]) -> (i32, String) {
    let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
    match kernel.execute_kj_quiet(context, &argv).await {
        Ok(result) => (result.exit_code, format!("{}{}", result.stdout, result.stderr)),
        Err(e) => (-1, e.to_string()),
    }
}

/// Over the wire, the person's archive of their console asks, with the
/// person as its reviewer; an allow runs the archive and a deny leaves the
/// console live. `--no-root` asks too. Falsified by dropping the gate's
/// always-ask arm: the archive runs at once, with no ask.
#[test]
fn a_person_s_archive_of_the_console_asks_and_runs_on_allow() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (handle, _) = client.bind_kernel().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &handle.list_contexts().await.unwrap()).unwrap().context_id;
        let person = server.kernel_db.lock().get_context(root).unwrap().unwrap().performer.expect("a person performs the root console");
        let name = server.kernel_db.lock().name_for(person);
        let pending = || server.kernel_db.lock().list_pending_asks().unwrap();
        let live = |ctx: ContextId| !server.kernel_db.lock().get_context(ctx).unwrap().unwrap().is_archived();

        let mut consoles = Vec::new();
        for verb in ["deny", "allow"] {
            let con = create_context(&handle, &format!("con-{verb}")).await.unwrap();
            let (code, out) = kj(&handle, root, &["character", "set", &name, "--console", &con.to_hex()]).await;
            assert_eq!(code, 0, "{out}");
            assert!(pending().is_empty(), "the person's own setup runs without an ask");

            let (code, out) = kj(&handle, root, &["context", "archive", &con.to_hex(), "--confirm"]).await;
            assert_ne!(code, 0, "the archive waits for its ask: {out}");
            assert!(live(con), "nothing ran before the answer");
            let ask = pending().pop().expect("the archive of the console raised an ask");
            assert_eq!(ask.reviewer_id.as_deref(), Some(person.as_bytes().as_slice()), "the person answers their own ask");
            assert!(ask.description.contains("console"), "{}", ask.description);

            let (code, out) = kj(&handle, root, &["ledger", verb, &ask.request_id]).await;
            assert_eq!(code, 0, "{out}");
            consoles.push(con);
        }
        let [denied, allowed] = [consoles[0], consoles[1]];
        wait_for("the approved archive of the console", || !live(allowed)).await;
        assert!(live(denied), "a denied archive runs nothing");

        let (code, out) = kj(&handle, root, &["character", "set", &name, "--no-root", "--rc", "coder"]).await;
        assert_ne!(code, 0, "--no-root waits for its ask: {out}");
        let ask = pending().pop().expect("--no-root raised an ask");
        assert!(ask.description.contains("--no-root"), "{}", ask.description);
        assert!(server.kernel_db.lock().get_character(person).unwrap().unwrap().root, "nothing ran before the answer");
        let (code, out) = kj(&handle, root, &["ledger", "deny", &ask.request_id]).await;
        assert_eq!(code, 0, "{out}");

        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
