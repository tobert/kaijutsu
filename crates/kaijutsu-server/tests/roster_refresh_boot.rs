//! The roster's scheduled-periodic refresh is wired into server boot.
//!
//! `roster_sources::spawn_periodic_refresh` shipped with slice 2 fully
//! unit-tested and **called from nowhere** — the branch that built it could
//! not start a live kernel, so the one production call site was deliberately
//! left out (docs/issues.md, "Live roster — two things landed without, on
//! purpose"). That is a shape unit tests structurally cannot catch: the
//! function is correct, its caller is absent, and every test of the function
//! still passes.
//!
//! So these two tests assert the *wiring*, not the loop's behaviour:
//!
//! 1. The refresh runs at boot without anything reading the roster. This is
//!    what fails if the `tokio::spawn` in `create_shared_kernel` is ever
//!    deleted or moved behind a condition. It deliberately does NOT touch a
//!    read surface — `kj roster list` and `/run/roster` self-heal the boot
//!    rule inline via `kj/roster.rs::ensure_refreshed`, so a test that read
//!    one would pass with the spawn removed. Reading nothing is the point.
//! 2. Dropping the last `SharedKernelState` cancels the loop. The task holds
//!    `Arc<Kernel>`/`Arc<KernelDb>`/`Arc<RosterStore>` and deliberately not an
//!    `Arc<SharedKernelState>`; if someone ever "tidies" that into holding the
//!    shared state, the cycle would keep `Drop` from running and this test
//!    would catch it.

use std::time::Duration;

/// Boot spawns the refresh loop, and its first tick lands without any reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_refreshes_the_roster_without_a_reader() {
    let tmp = tempfile::tempdir().unwrap();

    let shared = kaijutsu_server::rpc::create_shared_kernel(None, Some(tmp.path()))
        .await
        .expect("create_shared_kernel should succeed on an empty data dir");

    // `refreshed_at()` is `None` on a fresh store until a `refresh_once`
    // completes — that is the boot rule's own marker (see `roster_sources`'s
    // module doc). Poll rather than sleep-once: `tokio::time::interval` fires
    // its first tick immediately, but "immediately" still means "after the
    // spawned task gets polled", so a bare sleep would be tuned to the
    // scheduler rather than to the contract.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if shared.roster.refreshed_at().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "roster was never refreshed at boot — is \
             roster_sources::spawn_periodic_refresh still called from \
             create_shared_kernel? Nothing in this test reads the roster, so \
             the read surfaces' inline ensure_refreshed cannot cover for a \
             missing spawn."
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Dropping the shared kernel cancels kernel-lifetime tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_shared_kernel_cancels_the_refresh_loop() {
    let tmp = tempfile::tempdir().unwrap();

    let shared = kaijutsu_server::rpc::create_shared_kernel(None, Some(tmp.path()))
        .await
        .expect("create_shared_kernel should succeed on an empty data dir");

    // Clone the token out so we can still observe it after the state is gone.
    let shutdown = shared.shutdown.clone();
    assert!(
        !shutdown.is_cancelled(),
        "shutdown token must be live while the kernel is"
    );

    drop(shared);

    assert!(
        shutdown.is_cancelled(),
        "dropping the last SharedKernelState must cancel kernel-lifetime \
         tasks — if this fails, something is holding an \
         Arc<SharedKernelState> (a cycle), and Drop never ran"
    );
}
