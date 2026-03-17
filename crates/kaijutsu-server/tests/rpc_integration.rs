//! Integration tests for kaijutsu RPC over SSH
//!
//! Uses ephemeral SSH keys generated in memory for testing.

mod common;
use common::*;

#[test]
fn test_whoami() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let identity = client.whoami().await.unwrap();
        assert_eq!(identity.username, "test_user");
        assert_eq!(identity.display_name, "test_user");
    });
}

#[test]
fn test_list_kernels_shows_shared_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // The shared kernel is created at server startup — always one kernel
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1, "Expected one shared kernel");
    });
}

#[test]
fn test_attach_kernel_creates_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel (server auto-creates)
        let (kernel, kernel_id) = client.attach_kernel().await.unwrap();
        let info = kernel.get_info().await.unwrap();
        assert!(!kernel_id.is_nil());
        assert_eq!(info.id, kernel_id);
    });
}

#[test]
fn test_kernel_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel
        let (_kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Check it appears in list
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1);
    });
}

#[test]
fn test_create_context_returns_valid_id() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let context_id = kernel.create_context("test-ctx").await.unwrap();

        assert!(!context_id.is_nil(), "createContext should return a non-nil ContextId");
    });
}

#[test]
fn test_create_context_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Count contexts before
        let before = kernel.list_contexts().await.unwrap();
        let before_count = before.len();

        let context_id = kernel.create_context("my-label").await.unwrap();

        // Should appear in list with correct label
        let after = kernel.list_contexts().await.unwrap();
        assert_eq!(after.len(), before_count + 1, "New context should appear in list");

        let found = after.iter().find(|c| c.id == context_id);
        assert!(found.is_some(), "Created context should be findable by ID");
        assert_eq!(found.unwrap().label, "my-label");
    });
}

#[test]
fn test_create_context_joinable() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let context_id = kernel.create_context("joinable").await.unwrap();

        // Should be joinable
        let joined_id = kernel.join_context(context_id, "test-instance").await.unwrap();
        assert_eq!(joined_id, context_id, "Joining a created context should return the same ID");
    });
}

#[test]
fn test_join_nonexistent_context_fails() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Joining a random context that was never created should fail
        let random_id = kaijutsu_crdt::ContextId::new();
        let result = kernel.join_context(random_id, "test-instance").await;
        assert!(result.is_err(), "join_context with nonexistent ID should fail");
    });
}

#[test]
fn test_create_context_unique_ids() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let id1 = kernel.create_context("ctx-a").await.unwrap();
        let id2 = kernel.create_context("ctx-b").await.unwrap();

        assert_ne!(id1, id2, "Each created context should have a unique ID");
    });
}
