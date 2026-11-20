//! e2e: **sol says when it runs off its model.** Sol's stance is written
//! for GPT-6.2. Its create lifecycle (`S12-model-check.kai`) leaves a
//! notification naming the commands that move sol onto a GPT model when the
//! context runs any other model, or none, and stays quiet on a GPT model.

mod common;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{KernelHandle, choose_parent};
use kaijutsu_types::{BlockKind, BlockQuery, ContextId};

/// Run `kj <argv>` from the kernel's root context and panic on failure.
async fn kj_ok(kj: &KernelHandle, argv: &[&str]) {
    let parent = choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap();
    let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
    let result = kj.execute_kj_quiet(parent.context_id, &argv).await.unwrap();
    assert_eq!(result.exit_code, 0, "kj {argv:?}: {}", result.stderr);
}

/// The model notice in `context`, if its create lifecycle wrote one.
async fn model_notice(kj: &KernelHandle, context: ContextId) -> Option<String> {
    kj.get_blocks(context, &BlockQuery::All).await.unwrap().into_iter()
        .find(|b| b.kind == BlockKind::Notification && b.content.contains("written for GPT-6.2"))
        .map(|b| b.content)
}

#[test]
fn sol_names_the_setup_when_it_runs_off_a_gpt_model() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let sol = common::rc_performer(&kj, "sol").await.unwrap();

        let unbound = common::create_context_as(&kj, "sol-unbound", &sol).await.unwrap();
        let notice = model_notice(&kj, unbound).await.expect("a context with no model gets the notice");
        for line in [
            "kj backend set chatgpt --kind chatgpt".to_string(),
            "kj backend login chatgpt".to_string(),
            format!("kj character model set {sol} chatgpt/gpt-6.2-sol"),
            "kj context rotate sol-unbound".to_string(),
        ] {
            assert!(notice.contains(&line), "the notice names {line:?}:\n{notice}");
        }

        kj_ok(&kj, &["backend", "set", "mock", "--kind", "mock", "--key-optional"]).await;
        kj_ok(&kj, &["character", "model", "set", &sol, "mock/mock-model", "--max-tokens", "1024"]).await;
        let off = common::create_context_as(&kj, "sol-off", &sol).await.unwrap();
        let notice = model_notice(&kj, off).await.expect("a context on another model gets the notice");
        assert!(notice.contains("mock/mock-model") && notice.contains("kj context rotate sol-off"), "{notice}");
        assert!(!notice.contains('\\'), "the notice carries no stray escapes:\n{notice}");

        kj_ok(&kj, &["backend", "set", "chatgpt", "--kind", "chatgpt"]).await;
        kj_ok(&kj, &["character", "model", "set", &sol, "chatgpt/gpt-6.2-sol"]).await;
        let on = common::create_context_as(&kj, "sol-on", &sol).await.unwrap();
        assert_eq!(model_notice(&kj, on).await, None, "a context on a GPT model gets no notice");
    });
}
