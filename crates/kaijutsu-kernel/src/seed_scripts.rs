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
//! The script bodies live as real files under `assets/defaults/rc/` (a
//! 1:1 mirror of the `/etc/rc` tree), embedded here via `include_str!`.
//! Edit the asset file to change what fresh DBs get — but not what
//! already-seeded DBs have (paths collide, INSERT OR IGNORE skips).
//! `kj rc reseed` is the explicit push-updates path: it overwrites
//! matching paths from these embedded defaults.

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
// Bodies live as real files under `assets/defaults/rc/` (a 1:1 mirror of
// the `/etc/rc` tree) so they can be edited with an external editor and
// reviewed as files. `include_str!` embeds them at build time. Shared
// recipes (cache, permissive binding) appear at multiple `/etc/rc` paths;
// each const points at one representative copy in the mirror.
const CACHE_CREATE_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/create/S20-cache.kai");

const CACHE_FORK_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/fork/S30-cache.kai");

const CACHE_DRIFT_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/drift/S40-cache.kai");

/// The coder context_type's system-prompt stance. Drawn from the
/// project's cybernetic / kaizen directives — restated in-kernel so the
/// contract doesn't depend on which client connected.
const CODER_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/coder/create/S00-stance.md");

/// The `mcp` context_type's stance. This is the default mode for a context
/// born from `register_session` — an external agent (Claude Code, Gemini CLI,
/// opencode) driving the kernel over the narrow MCP surface, with
/// `context_shell` as the entry point and `kj` as the rich command surface.
const MCP_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/mcp/create/S00-stance.md");

/// The explorer context_type's stance: a read-only role for investigation
/// without mutation. Pairs with the capability allow-set below — the stance
/// tells the model what it is; the binding enforces it.
const EXPLORER_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/explorer/create/S00-stance.md");

/// The broad loadout for human-facing / general roles (`default`, `coder`,
/// `mcp`). Deny-by-default everywhere, so permissiveness is explicit: `*` =
/// every instance, `facade:*` = every facade. Does **not** grant `admin` —
/// these roles can use everything but cannot rebind *other* contexts (that's
/// the director role). The rc lifecycle runs privileged, so this widen from
/// deny-all is allowed; an agent could not issue it at runtime.
const PERMISSIVE_BINDING_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/create/S10-binding.kai");

/// explorer capability allow-set. Deny-by-default means this enumerates
/// exactly the read-oriented tools the role may use; everything else is
/// refused at `call_tool`. No facades — shell/edit/submit are withheld, and
/// reading the compose buffer (`get_input_state`) is ungated, so a read-only
/// role needs no facade grant. Tokens are quoted because the kaish lexer
/// special-cases bare words containing `.`/`:`.
const EXPLORER_BINDING_BODY: &str =
    include_str!("../../../assets/defaults/rc/explorer/create/S10-binding.kai");

/// The director context_type's stance: a coordination role that owns block
/// tooling and binding administration but not raw file writes.
const DIRECTOR_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/director/create/S00-stance.md");

/// director capability allow-set: full block tooling + read + binding admin.
/// `admin` is the binding-admin capability — a director may write *any*
/// context's loadout (manage other contexts), which broad roles cannot.
const DIRECTOR_BINDING_BODY: &str =
    include_str!("../../../assets/defaults/rc/director/create/S10-binding.kai");

const SEED_SCRIPTS: &[SeedScript] = &[
    // ── default context_type — broad loadout + cache recipe ─────────────
    // S10 must precede any rc script that calls a broker tool: deny-by-default
    // means tool calls before the loadout is assigned would be refused.
    SeedScript {
        path: "/etc/rc/default/create/S10-binding.kai",
        context_type: "default",
        verb: "create",
        sort_key: "S10",
        name: "binding",
        extension: "kai",
        content: PERMISSIVE_BINDING_BODY,
        timeout_secs: None,
    },
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
    // ── coder context_type — stance + broad loadout + cache ─────────────
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
        path: "/etc/rc/coder/create/S10-binding.kai",
        context_type: "coder",
        verb: "create",
        sort_key: "S10",
        name: "binding",
        extension: "kai",
        content: PERMISSIVE_BINDING_BODY,
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
    // ── mcp context_type — stance + broad loadout + cache (register_session) ──
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
        path: "/etc/rc/mcp/create/S10-binding.kai",
        context_type: "mcp",
        verb: "create",
        sort_key: "S10",
        name: "binding",
        extension: "kai",
        content: PERMISSIVE_BINDING_BODY,
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
    // ── explorer context_type — read-only role (stance + binding + cache) ──
    SeedScript {
        path: "/etc/rc/explorer/create/S00-stance.md",
        context_type: "explorer",
        verb: "create",
        sort_key: "S00",
        name: "stance",
        extension: "md",
        content: EXPLORER_STANCE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/explorer/create/S10-binding.kai",
        context_type: "explorer",
        verb: "create",
        sort_key: "S10",
        name: "binding",
        extension: "kai",
        content: EXPLORER_BINDING_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/explorer/create/S20-cache.kai",
        context_type: "explorer",
        verb: "create",
        sort_key: "S20",
        name: "cache",
        extension: "kai",
        content: CACHE_CREATE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/explorer/fork/S30-cache.kai",
        context_type: "explorer",
        verb: "fork",
        sort_key: "S30",
        name: "cache",
        extension: "kai",
        content: CACHE_FORK_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/explorer/drift/S40-cache.kai",
        context_type: "explorer",
        verb: "drift",
        sort_key: "S40",
        name: "cache",
        extension: "kai",
        content: CACHE_DRIFT_BODY,
        timeout_secs: None,
    },
    // ── director context_type — coordination role (stance + binding + cache) ──
    SeedScript {
        path: "/etc/rc/director/create/S00-stance.md",
        context_type: "director",
        verb: "create",
        sort_key: "S00",
        name: "stance",
        extension: "md",
        content: DIRECTOR_STANCE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/director/create/S10-binding.kai",
        context_type: "director",
        verb: "create",
        sort_key: "S10",
        name: "binding",
        extension: "kai",
        content: DIRECTOR_BINDING_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/director/create/S20-cache.kai",
        context_type: "director",
        verb: "create",
        sort_key: "S20",
        name: "cache",
        extension: "kai",
        content: CACHE_CREATE_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/director/fork/S30-cache.kai",
        context_type: "director",
        verb: "fork",
        sort_key: "S30",
        name: "cache",
        extension: "kai",
        content: CACHE_FORK_BODY,
        timeout_secs: None,
    },
    SeedScript {
        path: "/etc/rc/director/drift/S40-cache.kai",
        context_type: "director",
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
