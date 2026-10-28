//! e2e: **a coder reads its project's `AGENTS.md`.** The coder create
//! lifecycle (`S36-house-rules.kai`) writes the nearest `AGENTS.md` at or
//! above the context's cwd, up to the repository root, as a cached system
//! instruction. `docs/prompts.md`, "Project house rules".

mod common;

use std::path::Path;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{choose_parent, context_create_argv, context_id_from_create_result};
use kaijutsu_types::{BlockKind, BlockQuery, BlockSnapshot, Role};

/// Create a coder in `cwd` and return its house-rules block, if the create
/// lifecycle wrote one.
async fn house_rules_block(label: &str, cwd: &Path) -> Option<BlockSnapshot> {
    let addr = start_server().await;
    let client = connect_client(addr).await;
    let (kj, _) = client.bind_kernel().await.unwrap();
    let parent = choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap();
    let argv = context_create_argv(label, "coder", common::PERFORMER, Some(cwd.to_str().unwrap()));
    let result = kj.execute_kj_quiet(parent.context_id, &argv).await.unwrap();
    let coder = context_id_from_create_result(&result).expect("kj context create succeeds");
    kj.get_blocks(coder, &BlockQuery::All).await.unwrap()
        .into_iter().find(|b| b.content.starts_with("House rules from "))
}

/// A coder created in a project with an `AGENTS.md` gets the file as a cached
/// system instruction, the way other coding harnesses read it.
#[test]
fn a_coder_reads_the_agents_md_in_its_cwd() {
    run_local(async {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("AGENTS.md"), "# Rules\n\nhouse-rule-marker-7f3a: commit small steps.\n").unwrap();
        let block = house_rules_block("house-rules-cwd", cwd.path()).await
            .unwrap_or_else(|| panic!("no house-rules block for a cwd with AGENTS.md"));
        assert_eq!(block.role, Role::System, "house rules are a system block");
        assert_eq!(block.kind, BlockKind::Text, "house rules are an instruction, not a notification");
        assert!(
            block.content.contains("house-rule-marker-7f3a")
                && block.content.contains(&cwd.path().join("AGENTS.md").display().to_string()),
            "the block names the file and holds its text:\n{}", block.content
        );
    });
}

/// From a subdirectory, the nearest `AGENTS.md` up to the repository root
/// applies; one above the repository belongs to another project.
#[test]
fn a_coder_reads_agents_md_up_to_the_repository_root_and_no_further() {
    run_local(async {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("AGENTS.md"), "outer-marker-c41d\n").unwrap();
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("crates").join("one");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(
            house_rules_block("house-rules-none-in-repo", &sub).await.is_none(),
            "an AGENTS.md above the repository root must not apply"
        );

        std::fs::write(repo.join("AGENTS.md"), "repo-marker-9be2\n").unwrap();
        let block = house_rules_block("house-rules-repo-root", &sub).await
            .unwrap_or_else(|| panic!("no house-rules block from a subdirectory of a repo with AGENTS.md"));
        assert!(
            block.content.contains("repo-marker-9be2") && !block.content.contains("outer-marker-c41d"),
            "the repository's AGENTS.md applies, not the outer one:\n{}", block.content
        );
    });
}

/// A long `AGENTS.md` is cut at the cap, and the block says so.
#[test]
fn a_long_agents_md_is_cut_and_says_so() {
    run_local(async {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("AGENTS.md"), "x".repeat(40_000)).unwrap();
        let block = house_rules_block("house-rules-long", cwd.path()).await
            .unwrap_or_else(|| panic!("no house-rules block for a long AGENTS.md"));
        assert!(block.content.len() < 33_000, "the block is bounded: {} bytes", block.content.len());
        assert!(block.content.contains("[house rules cut at 32000 characters]"), "the cut is visible");
    });
}
