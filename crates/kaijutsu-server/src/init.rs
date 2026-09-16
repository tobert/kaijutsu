//! `kaijutsu-server init`: the person who runs a kernel creates themself
//! before the first connection (`docs/character.md`, "Bootstrap: the person
//! creates themself").
//!
//! `init` makes one root character and binds one key to it. It writes
//! `kernel.db` and `auth.db` directly, so run it with the service stopped.
//! The kernel creates the root context at its next start.

use kaijutsu_kernel::kernel_db::{CharacterRow, KernelDb};
use kaijutsu_types::PrincipalId;
use russh::keys::ssh_key::{HashAlg, PublicKey};

use crate::auth_db::AuthDb;

/// What `init` did.
#[derive(Debug)]
pub struct InitReport {
    pub character: CharacterRow,
    pub fingerprint: String,
    /// The character row was created or made a root.
    pub changed_character: bool,
    /// The key was bound by this run.
    pub bound_key: bool,
}

/// Make `name` the kernel's root character and bind `key` to it.
///
/// Checks everything before writing anything: a different live root, a
/// retired `name`, or a key bound to another character refuses with no
/// change. Repeating a completed `init` changes nothing.
pub fn init_root(
    kernel_db: &KernelDb,
    auth_db: &AuthDb,
    name: &str,
    key: &PublicKey,
    comment: Option<&str>,
) -> Result<InitReport, String> {
    let other_roots: Vec<String> = kernel_db
        .list_characters(false)
        .map_err(|e| format!("could not read characters: {e}"))?
        .into_iter()
        .filter(|row| row.root && row.name != name)
        .map(|row| row.name)
        .collect();
    if !other_roots.is_empty() {
        return Err(format!(
            "this kernel already has a root character ({}). Connect as it and run \
             `kj character create {name} --root`, then bind a key with \
             `kaijutsu-server add-key <pubkey-file> --as {name}`",
            other_roots.join(", ")
        ));
    }

    let existing = kernel_db
        .get_character_by_name(name)
        .map_err(|e| format!("could not read character '{name}': {e}"))?;
    if let Some(row) = &existing
        && row.retired_at.is_some()
    {
        return Err(format!("character '{name}' is retired; choose another name"));
    }

    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let bound_to = auth_db
        .get_key(&fingerprint)
        .map_err(|e| format!("could not read auth database: {e}"))?
        .map(|record| record.principal_id);
    if let (Some(owner), Some(row)) = (bound_to, &existing)
        && owner == row.principal_id
    {
        // Bound to this character already; fall through to the root flag.
    } else if let Some(owner) = bound_to {
        let owner_name = kernel_db
            .get_character(owner)
            .ok()
            .flatten()
            .map(|row| row.name)
            .unwrap_or_else(|| owner.short());
        return Err(format!(
            "key {fingerprint} is bound to {owner_name}. Use another key, or move it \
             after init with `kaijutsu-server add-key <pubkey-file> --as {name} --rebind`"
        ));
    }

    let (character, changed_character) = match existing {
        Some(row) if row.root => (row, false),
        Some(row) => {
            kernel_db
                .update_character_root(row.principal_id, true)
                .map_err(|e| format!("could not make '{name}' a root: {e}"))?;
            (CharacterRow { root: true, ..row }, true)
        }
        None => {
            let row = CharacterRow {
                principal_id: PrincipalId::new(),
                name: name.to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                retired_at: None,
                handoff_ctx: None,
                root_ctx: None,
                root: true,
            };
            kernel_db
                .insert_character(&row)
                .map_err(|e| format!("could not create '{name}': {e}"))?;
            (row, true)
        }
    };

    let bound_key = bound_to.is_none();
    if bound_key {
        auth_db
            .add_key(character.principal_id, key, comment)
            .map_err(|e| {
                format!(
                    "created root character '{name}' but could not bind the key: {e}. \
                     Run the same init again to finish"
                )
            })?;
    }

    Ok(InitReport { character, fingerprint, changed_character, bound_key })
}
