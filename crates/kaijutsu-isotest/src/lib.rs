//! kaijutsu-isotest — containerized isolation/functional tests.
//!
//! This crate is all `tests/`; there is no library. The suite runs inside a
//! podman container driven by `contrib/isotest`: an empty PID namespace where
//! the test binary spawns the real `kaijutsu-server` binary as a child,
//! drives it over loopback SSH with `kaijutsu-client`, kills it for real,
//! and asserts on `/proc`. See `docs/isotest.md` for the rationale and the
//! honest limits (what PDEATHSIG coverage here does and does not prove).
