//! Built-in rc lifecycle scripts seeded into the kernel DB on open.
//!
//! These are the "floor" that every kernel boots with. Two purposes:
//!
//! - `/etc/rc/default/{create,fork,drift}/*-cache.kai` — the prompt-cache
//!   recipe documented in `crates/kaijutsu-kernel/docs/help/kj-cache.md`,
//!   applied to every context that doesn't opt into a different
//!   `context_type`. Without this seed, fresh kernels miss all cache
//!   breakpoints until the user installs them by hand.
//! - `/etc/rc/coder/**` — the worked example of a real context_type.
//!   Includes an `S00-stance.md` so the kernel-side coding contract is
//!   self-contained (independent of any per-client CLAUDE.md), plus the
//!   same cache recipe as `default`.
//!
//! ## Seed contract
//!
//! `ensure_seeded_rc_scripts` runs at `KernelDb::open` / `in_memory` via
//! `INSERT OR IGNORE` on the rc_scripts unique-path index. Effects:
//!
//! - Fresh DB: every seed row inserts.
//! - Re-open with seeds intact: no-op (path collision ignored).
//! - User rm'd a seed: it reappears on the next open. This is intentional
//!   — the seed is the floor, not a one-time gift. To override a seed,
//!   use `kj rc edit` (replaces content in place) rather than `kj rc rm`.
//!
//! ## Updating the seed
//!
//! Editing a constant here changes what fresh DBs get, but not what
//! already-seeded DBs have (paths collide, INSERT OR IGNORE skips).
//! `kj rc reseed` is the explicit push-updates path: it overwrites
//! matching paths from these constants.

use rusqlite::{params, Connection, Result as SqliteResult};

use kaijutsu_types::PrincipalId;

use crate::kernel_db::blob_param;

/// One entry in the seed table. `timeout_secs = None` lets the kernel
/// default apply (only meaningful for `.kai` rows; `.md` ignores it).
struct SeedScript {
    path: &'static str,
    context_type: &'static str,
    verb: &'static str,
    sort_key: &'static str,
    name: &'static str,
    extension: &'static str,
    content: &'static str,
    timeout_secs: Option<u32>,
}

/// Universal cache recipe from `docs/help/kj-cache.md`. Shared between
/// `default` and `coder` because every conversational context wants
/// these breakpoints; the duplication is fine until a third consumer
/// shows up.
const CACHE_CREATE_BODY: &str = "\
# rc on-create cache breakpoints (docs/help/kj-cache.md).
# Tools array is fixed per session → 1h cache.
# System prompt may shift on rc edits / model swaps → 5m.
# --flag=value because the kj tool schema does not declare these flag
# names; bare --flag args otherwise parse as bool flags in kaish.
kj cache add --target=tools  --ttl=extended
kj cache add --target=system --ttl=ephemeral
";

const CACHE_FORK_BODY: &str = "\
# rc on-fork cache breakpoint: cache the prefix shared with the parent.
# KJ_PARENT_BLOCK_COUNT is the parent's block count at fork time, so
# index N-1 is the last shared message. --flag=value because the kj
# tool schema does not declare these flag names; bare --flag args
# otherwise parse as bool flags in kaish.
kj cache add --target=message --index=$((KJ_PARENT_BLOCK_COUNT - 1)) --ttl=extended
";

const CACHE_DRIFT_BODY: &str = "\
# rc on-drift cache reset: compact / model swap / doc inject reshape
# the conversation, so old MessageIndex breakpoints point at the wrong
# message now. Clear and re-seed the stable bits. --flag=value because
# the kj tool schema does not declare these flag names; bare --flag args
# otherwise parse as bool flags in kaish.
kj cache clear
kj cache add --target=tools  --ttl=extended
kj cache add --target=system --ttl=ephemeral
";

/// The coder context_type's system-prompt stance. Drawn from the
/// project's cybernetic / kaizen directives — restated in-kernel so the
/// contract doesn't depend on which client connected.
const CODER_STANCE_BODY: &str = "\
You are coding inside kaijutsu — a cybernetic system for multi-user,
multi-model, multi-context collaboration. We work as equals: ask
clarifying questions, push back when a prompt is ambiguous, name
another option when one exists.

The standard we walk by is the standard we accept (改善). Note problems
we can fix later in auto-memory or the active plan; then move on.

Test-driven: write tests that can and will fail when we make mistakes.
Crash on data corruption; silent fallbacks are usually a mistake.

We do not seek a single root cause — describe contributing factors and
the system shape that admitted the failure.

Don't add features, refactors, or abstractions the task didn't ask for.
Edit existing files rather than creating new ones when you can.

`kj` is your lever inside the kernel: fork to explore safely, drift to
share findings between contexts, cache to amortize prompt-token cost,
rc to install lifecycle scripts, block to inspect the conversation.
";

/// The `mcp` context_type's stance. This is the default mode for a context
/// born from `register_session` — an external agent (Claude Code, Gemini CLI,
/// opencode) driving the kernel over the narrow MCP surface, with
/// `context_shell` as the entry point and `kj` as the rich command surface.
const MCP_STANCE_BODY: &str = "\
You are driving a kaijutsu context from outside the kernel, over MCP.
`context_shell` is your entry point; everything rich happens by running
`kj …` and shell commands through it. We work as equals: ask clarifying
questions, push back when a prompt is ambiguous, name another option.

The standard we walk by is the standard we accept (改善). Note problems
we can fix later in auto-memory or the active plan; then move on.

`kj` is the kernel's command surface: `kj context` to see where you are,
`kj fork` to explore safely, `kj drift` to share findings between
contexts, `kj block` to inspect the conversation, `kj help` for the rest.
Prefer `kj` over guessing — it carries structured `--json` output.
";

const SEED_SCRIPTS: &[SeedScript] = &[
    // ── default context_type — cache recipe only ───────────────────────
    SeedScript {
        path: "/etc/rc/default/create/S20-cache.kai",
        context_type: "default",
        verb: "create",
        sort_key: "S20",
        name: "cache",
        extension: "kai",
        content: CACHE_CREATE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/default/fork/S30-cache.kai",
        context_type: "default",
        verb: "fork",
        sort_key: "S30",
        name: "cache",
        extension: "kai",
        content: CACHE_FORK_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/default/drift/S40-cache.kai",
        context_type: "default",
        verb: "drift",
        sort_key: "S40",
        name: "cache",
        extension: "kai",
        content: CACHE_DRIFT_BODY,
        timeout_secs: None,
    },
    // ── coder context_type — stance + cache ────────────────────────────
    SeedScript {
        path: "/etc/rc/coder/create/S00-stance.md",
        context_type: "coder",
        verb: "create",
        sort_key: "S00",
        name: "stance",
        extension: "md",
        content: CODER_STANCE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/coder/create/S20-cache.kai",
        context_type: "coder",
        verb: "create",
        sort_key: "S20",
        name: "cache",
        extension: "kai",
        content: CACHE_CREATE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/coder/fork/S30-cache.kai",
        context_type: "coder",
        verb: "fork",
        sort_key: "S30",
        name: "cache",
        extension: "kai",
        content: CACHE_FORK_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/coder/drift/S40-cache.kai",
        context_type: "coder",
        verb: "drift",
        sort_key: "S40",
        name: "cache",
        extension: "kai",
        content: CACHE_DRIFT_BODY,
        timeout_secs: None,
    },
    // ── mcp context_type — stance + cache (default for register_session) ──
    SeedScript {
        path: "/etc/rc/mcp/create/S00-stance.md",
        context_type: "mcp",
        verb: "create",
        sort_key: "S00",
        name: "stance",
        extension: "md",
        content: MCP_STANCE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/mcp/create/S20-cache.kai",
        context_type: "mcp",
        verb: "create",
        sort_key: "S20",
        name: "cache",
        extension: "kai",
        content: CACHE_CREATE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/mcp/fork/S30-cache.kai",
        context_type: "mcp",
        verb: "fork",
        sort_key: "S30",
        name: "cache",
        extension: "kai",
        content: CACHE_FORK_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/mcp/drift/S40-cache.kai",
        context_type: "mcp",
        verb: "drift",
        sort_key: "S40",
        name: "cache",
        extension: "kai",
        content: CACHE_DRIFT_BODY,
        timeout_secs: None,
    },
];

/// Apply the in-code seed scripts to a freshly-opened DB. Idempotent
/// via `INSERT OR IGNORE` on the rc_scripts unique-path constraint:
/// reseed-on-every-open is the floor contract.
pub(crate) fn ensure_seeded_rc_scripts(conn: &Connection) -> SqliteResult<()> {
    let founder = PrincipalId::system();
    let now = kaijutsu_types::now_millis() as i64;
    for seed in SEED_SCRIPTS {
        conn.execute(
            "INSERT OR IGNORE INTO rc_scripts (
                context_type, verb, sort_key, name,
                extension, content, path,
                created_at, created_by, timeout_secs
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                seed.context_type,
                seed.verb,
                seed.sort_key,
                seed.name,
                seed.extension,
                seed.content,
                seed.path,
                now,
                blob_param(founder.as_bytes()),
                seed.timeout_secs,
            ],
        )?;
    }
    Ok(())
}

/// Force-overwrite seed rows from the in-code defaults. Powers
/// `kj rc reseed`. Uses INSERT OR REPLACE so user edits to seeded
/// paths get reverted; non-seed paths are untouched.
///
/// `type_filter`, if `Some`, narrows the reseed to one context_type.
/// Returns the number of paths reseeded.
pub(crate) fn reseed_rc_scripts(
    conn: &Connection,
    type_filter: Option<&str>,
) -> SqliteResult<usize> {
    let founder = PrincipalId::system();
    let now = kaijutsu_types::now_millis() as i64;
    let mut count = 0usize;
    for seed in SEED_SCRIPTS {
        if let Some(t) = type_filter {
            if seed.context_type != t {
                continue;
            }
        }
        conn.execute(
            "INSERT OR REPLACE INTO rc_scripts (
                context_type, verb, sort_key, name,
                extension, content, path,
                created_at, created_by, timeout_secs
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                seed.context_type,
                seed.verb,
                seed.sort_key,
                seed.name,
                seed.extension,
                seed.content,
                seed.path,
                now,
                blob_param(founder.as_bytes()),
                seed.timeout_secs,
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

/// The set of context_types we ship seeds for. `kj rc reseed --type X`
/// rejects values outside this list so typos don't silently no-op.
pub(crate) fn seeded_context_types() -> Vec<&'static str> {
    let mut types: Vec<&'static str> = SEED_SCRIPTS.iter().map(|s| s.context_type).collect();
    types.sort();
    types.dedup();
    types
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::KernelDb;

    #[test]
    fn fresh_db_has_default_and_coder_seeds() {
        let db = KernelDb::in_memory().expect("in-memory db");
        // Default cache scripts present.
        for path in [
            "/etc/rc/default/create/S20-cache.kai",
            "/etc/rc/default/fork/S30-cache.kai",
            "/etc/rc/default/drift/S40-cache.kai",
        ] {
            assert!(
                db.get_rc_script(path).unwrap().is_some(),
                "missing default seed: {path}"
            );
        }
        // Coder stance + cache scripts present.
        for path in [
            "/etc/rc/coder/create/S00-stance.md",
            "/etc/rc/coder/create/S20-cache.kai",
            "/etc/rc/coder/fork/S30-cache.kai",
            "/etc/rc/coder/drift/S40-cache.kai",
        ] {
            assert!(
                db.get_rc_script(path).unwrap().is_some(),
                "missing coder seed: {path}"
            );
        }
    }

    #[test]
    fn seed_is_idempotent_user_edits_persist() {
        // Open, edit a seed, reopen the SAME DB (file-backed), confirm
        // edit survived (INSERT OR IGNORE didn't clobber it).
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("kernel.db");

        {
            let db = KernelDb::open(&path).expect("open 1");
            db.update_rc_script(
                "/etc/rc/default/create/S20-cache.kai",
                Some("# user-edited body"),
                None,
            )
            .expect("edit seed");
        }

        let db2 = KernelDb::open(&path).expect("open 2");
        let row = db2
            .get_rc_script("/etc/rc/default/create/S20-cache.kai")
            .expect("get")
            .expect("row exists");
        assert_eq!(
            row.content, "# user-edited body",
            "edit was clobbered by second open's seed"
        );
    }

    #[test]
    fn reseed_overwrites_user_edits() {
        let db = KernelDb::in_memory().expect("in-memory db");
        db.update_rc_script(
            "/etc/rc/default/create/S20-cache.kai",
            Some("# user override"),
            None,
        )
        .expect("edit seed");

        let count = db.reseed_rc_scripts(None).expect("reseed");
        assert!(count > 0, "reseed should touch >0 rows");

        let row = db
            .get_rc_script("/etc/rc/default/create/S20-cache.kai")
            .unwrap()
            .unwrap();
        assert!(
            row.content.contains("kj cache add --target=tools"),
            "reseed didn't restore the in-code body: {}",
            row.content
        );
    }

    #[test]
    fn reseed_with_type_filter_skips_others() {
        let db = KernelDb::in_memory().expect("in-memory db");
        // Edit both a default and a coder seed.
        db.update_rc_script(
            "/etc/rc/default/create/S20-cache.kai",
            Some("# default edited"),
            None,
        )
        .unwrap();
        db.update_rc_script(
            "/etc/rc/coder/create/S20-cache.kai",
            Some("# coder edited"),
            None,
        )
        .unwrap();

        // Reseed only coder.
        db.reseed_rc_scripts(Some("coder")).unwrap();

        // Coder restored.
        let coder = db
            .get_rc_script("/etc/rc/coder/create/S20-cache.kai")
            .unwrap()
            .unwrap();
        assert!(coder.content.contains("kj cache add"));
        // Default edit preserved.
        let default = db
            .get_rc_script("/etc/rc/default/create/S20-cache.kai")
            .unwrap()
            .unwrap();
        assert_eq!(default.content, "# default edited");
    }

    #[test]
    fn seeded_context_types_covers_both() {
        let types = seeded_context_types();
        assert!(types.contains(&"default"));
        assert!(types.contains(&"coder"));
    }
}
