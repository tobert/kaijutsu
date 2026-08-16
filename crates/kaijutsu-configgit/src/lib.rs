//! Git write seam for Lane B — rc/config documents becoming
//! plain files in a kernel-owned git worktree (`docs/config-ownership.md`,
//! "Lane B — the git-worktree seam, shipped and deliberately unwired").
//!
//! This crate is **not wired into the kernel**. It is the write half of the
//! future `<data_dir>/config` worktree, built and proven in isolation so the
//! kernel-wiring slice (a later patch) has a working seam to call instead of
//! a design to implement. See the module docs on [`Repo`] for the shape of
//! that seam and why it carries no kaijutsu types.
//!
//! ## Why gitoxide plumbing, never `gix`, never the `git` executable
//!
//! Kaijutsu's host-exec-has-one-owner rule (`CLAUDE.md`) means a `git`
//! subprocess here would be a second exec site alongside kaish's — this
//! crate never spawns one. The `gix` facade crate is avoided too: it links
//! `gix-command` unconditionally (even with `--no-default-features`) via
//! `gix-command -> gix-transport -> gix-protocol -> gix`, reintroducing
//! subprocess-spawn machinery through the back door. Instead this crate is
//! built directly on gitoxide's low-level plumbing crates — object encoding,
//! a loose object store writer, and ref transactions — none of which can
//! spawn a process. `tests/spawn_free.rs` is the tripwire that keeps that
//! true.
//!
//! The plumbing pins are matched to `~/src/kaish-extras`'s `kaish-tools-git`
//! crate (a sibling project, inspected 2026-08-15 at commit `96e9825`) so
//! that when kaish-extras grows a write profile, this seam can move there
//! instead of being reimplemented — see `Cargo.toml` for the exact pins and
//! the reasoning per crate.
//!
//! ## What this crate does not do
//!
//! No kernel `ContextId`/`BlockStore`, no `ConfigDocFs`
//! wiring, no migration of existing documents. It does not stage changes
//! through an on-disk git index — `commit_all` rewalks the live worktree
//! directory on every call and always commits its full current state, which
//! is what "auto-commit per accepted mutation" (`docs/config-ownership.md`,
//! "Rulings (Amy, 2026-08-15)", ruling 2) actually needs: the kernel's VFS is
//! the index, not git's.
//!
//! Unix-only: entry names are read as raw bytes via
//! [`std::os::unix::ffi::OsStrExt`], matching every other place this
//! worktree will run (`CLAUDE.md` "Machines" — moltar, zorak, both Unix; the
//! Bevy client is a separate cross-platform surface that never touches this
//! directory directly).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use gix_object::Write as _;
use gix_object::bstr::BString;
use gix_object::tree::{Entry as TreeEntry, EntryKind};
use gix_ref::Target;
use gix_ref::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};

/// The header key under which each commit's kernel operation id is stored,
/// as a commit-object extra header (alongside `tree`, `parent`, `author`,
/// `committer`). Real git ignores headers it doesn't recognize — `git log
/// --format=%(trailers)` won't show it (it isn't a trailer), but `git cat-file
/// -p <commit>` prints it verbatim, which is what makes this an
/// operator-visible join between a commit and the kernel operation that
/// caused it, not just an internal bookkeeping field. Keeping the id out of
/// the message body means the message stays human-authored prose and the
/// join key stays machine-parseable, instead of forcing a "Operation-Id:"
/// footer convention on every message.
const OPERATION_ID_HEADER: &str = "kaijutsu-operation-id";

/// Ruling 5 (`docs/config-ownership.md`, "Rulings (Amy, 2026-08-15)"):
/// commits are service-authored for now, and principal plumbing does not
/// gate this work. This name is the placeholder until that retrofit lands —
/// tracked as its own holistic sweep in `docs/issues.md` ("Principal
/// plumbing"), not a Lane B sub-task.
const SERVICE_AUTHOR_NAME: &str = "kaijutsu-kernel";
const SERVICE_AUTHOR_EMAIL: &str = "kernel@kaijutsu.internal";

/// The one branch this worktree ever commits to. Lane B's ruling is one git
/// worktree, one history, for all four config-like roots at once — there is
/// no per-root or per-mutation branching, so a single fixed branch name is
/// correct rather than a simplification to revisit later.
const BRANCH: &str = "refs/heads/main";

/// Errors from the git write seam. Every variant names the gitoxide-side
/// failure it wraps; nothing here is a kaijutsu kernel error type, which is
/// the point (see the crate docs on the thin-seam requirement).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error ({action}) at '{path}': {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("'{0}' exists but is not a git directory this seam can open (no HEAD file) — refusing to guess, per the Lane B 'fail loud' policy")]
    NotAGitDir(PathBuf),
    #[error("a worktree entry name is not valid on this platform: {0}")]
    InvalidEntryName(PathBuf),
    #[error("'{0}' is not a valid commit id")]
    InvalidCommitId(String),
    #[error("commit '{0}' was not found in the object database")]
    CommitNotFound(String),
    #[error("path '{path}' was not found in commit '{commit}'")]
    PathNotFound { commit: String, path: String },
    #[error("git object write failed: {0}")]
    ObjectWrite(#[from] gix_object::write::Error),
    #[error("git object lookup failed: {0}")]
    ObjectFind(#[from] gix_odb::loose::find::Error),
    #[error("git object decode failed: {0}")]
    ObjectDecode(#[from] gix_object::decode::Error),
    #[error("ref lookup failed: {0}")]
    RefFind(#[from] gix_ref::file::find::Error),
    #[error("ref transaction prepare failed: {0}")]
    RefPrepare(#[from] gix_ref::file::transaction::prepare::Error),
    #[error("ref transaction commit failed: {0}")]
    RefCommit(#[from] gix_ref::file::transaction::commit::Error),
}

impl Error {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Error::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A commit id, opaque outside this crate. Deliberately a hex `String`, not
/// a re-exported `gix_hash::ObjectId` — the thin-seam requirement is that
/// nothing gitoxide-shaped crosses the public boundary, so a caller (the
/// eventual kernel wiring) never needs a gitoxide dependency just to hold a
/// value this crate handed back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitId(String);

impl CommitId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn to_oid(&self) -> Result<gix_hash::ObjectId, Error> {
        gix_hash::ObjectId::from_hex(self.0.as_bytes())
            .map_err(|_| Error::InvalidCommitId(self.0.clone()))
    }
}

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A handle to the config worktree's git repository.
///
/// Carries only paths and gitoxide's own low-level store handles — no
/// kaijutsu kernel type appears in this struct or in any method signature on
/// it. That's what makes this "liftable into `kaish-tools-git` as a write
/// profile later without a rewrite" (`docs/config-ownership.md`, "Lane B
/// — the git-worktree seam, shipped and deliberately unwired"): the
/// kernel-wiring slice adapts kernel calls to this API, not the other way
/// around.
pub struct Repo {
    /// `<data_dir>/config` — the worktree root. `rc/`, `config/`, `client/`,
    /// `midi/` and everything else live here as ordinary files;
    /// `commit_all` walks this directory (minus `.git`) on every call.
    worktree_dir: PathBuf,
    /// `<worktree_dir>/.git`
    git_dir: PathBuf,
    odb: gix_odb::loose::Store,
    refs: gix_ref::file::Store,
    branch: gix_ref::FullName,
}

/// Create the worktree + repository at `worktree_dir` if it doesn't exist
/// yet, or open it if it does. Idempotent: calling this twice in a row on
/// the same path is the same as calling it once.
///
/// "Open" is deliberately narrow — it checks for `.git/HEAD` and nothing
/// more. Lane B's ruling (`docs/config-ownership.md`, "Rulings (Amy,
/// 2026-08-15)", ruling 3) is "no watcher, no implicit import; detect
/// unexpected dirtiness and fail loud", and an existing directory that lacks
/// `.git/HEAD` is exactly that: something this seam did not create and
/// should not silently adopt or overwrite.
pub fn init_or_open(worktree_dir: impl AsRef<Path>) -> Result<Repo, Error> {
    let worktree_dir = worktree_dir.as_ref().to_path_buf();
    let git_dir = worktree_dir.join(".git");

    if git_dir.join("HEAD").is_file() {
        // Already initialized by a previous call (or a previous process —
        // this seam has no in-memory-only state, everything durable lives
        // on disk under `.git`). Nothing to do.
    } else if git_dir.exists() {
        return Err(Error::NotAGitDir(git_dir));
    } else {
        init_repo_layout(&worktree_dir, &git_dir)?;
    }

    let odb = gix_odb::loose::Store::at(
        git_dir.join("objects"),
        gix_odb::loose::Options {
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    );
    let refs = gix_ref::file::Store::at(
        git_dir.clone(),
        gix_ref::store::init::Options {
            // Force a reflog on every ref update rather than git's normal
            // rules (which only start logging once `logs/HEAD` already
            // exists). This worktree is an operator-visible recovery
            // surface (`docs/config-ownership.md`, "Rulings (Amy,
            // 2026-08-15)", ruling 2); `git reflog` is exactly the kind of
            // thing an operator reaches for, and there is never a reason for
            // it to be empty here.
            write_reflog: gix_ref::store::WriteReflog::Always,
            object_hash: gix_hash::Kind::Sha1,
            precompose_unicode: false,
            prohibit_windows_device_names: false,
        },
    );
    let branch = gix_ref::FullName::try_from(BRANCH).expect("BRANCH is a valid ref name literal");

    Ok(Repo {
        worktree_dir,
        git_dir,
        odb,
        refs,
        branch,
    })
}

/// Hand-write the `.git` layout instead of pulling in a "git init" helper.
/// There isn't a plumbing-level one in the pins this crate uses (`gix-repository`
/// is the layer that would offer it, and it is not part of the aligned pin
/// set — see `Cargo.toml`) — but init is genuinely this small: two
/// directories, a `HEAD` symref, and a minimal `config` so real git can
/// recognize the directory if an operator ever points it there by hand.
fn init_repo_layout(worktree_dir: &Path, git_dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(worktree_dir).map_err(|e| Error::io("create worktree dir", worktree_dir, e))?;
    fs::create_dir_all(git_dir.join("objects")).map_err(|e| Error::io("create objects dir", git_dir, e))?;
    fs::create_dir_all(git_dir.join("refs").join("heads"))
        .map_err(|e| Error::io("create refs/heads dir", git_dir, e))?;

    let head_path = git_dir.join("HEAD");
    fs::write(&head_path, format!("ref: {BRANCH}\n")).map_err(|e| Error::io("write", &head_path, e))?;

    let config_path = git_dir.join("config");
    let config_body = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n";
    fs::write(&config_path, config_body).map_err(|e| Error::io("write", &config_path, e))?;

    Ok(())
}

impl Repo {
    /// The worktree root this repo was opened at.
    pub fn worktree_dir(&self) -> &Path {
        &self.worktree_dir
    }

    /// `<worktree_dir>/.git` — exposed for operator tooling and tests that
    /// want to inspect the on-disk layout directly (e.g. confirming
    /// `HEAD`/`refs/heads/main` exist) without this crate growing a second,
    /// redundant accessor API for the same information.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Stage the current worktree state and commit it.
    ///
    /// "Stage" is not an on-disk git index here — there is no `git add`
    /// step. Every call rewalks `worktree_dir` (skipping `.git`) and writes
    /// a fresh tree from what it finds, the same way `git commit -a` would
    /// look if the working directory were the only source of truth. That
    /// matches ruling 2: the git log is the config oplog, one commit per
    /// accepted kernel mutation, and the kernel's VFS — not a git index —
    /// is what already tracked "what changed" before this call.
    ///
    /// `operation_id` is carried as a commit extra header
    /// ([`OPERATION_ID_HEADER`]), not folded into `message`, so the message
    /// stays human-authored prose and the id stays a stable, greppable join
    /// key (`git log --grep`, `git cat-file -p`) between a commit and the
    /// kernel operation that produced it.
    ///
    /// The branch update is compare-and-swap against the ref's current
    /// value (`PreviousValue::MustExistAndMatch`/`MustNotExist`), not an
    /// unconditional overwrite — a concurrent writer racing this call fails
    /// the transaction rather than silently losing a commit, which is the
    /// "crashing is preferred over data corruption" posture applied to a
    /// git ref instead of a database row.
    pub fn commit_all(&self, message: &str, operation_id: &str) -> Result<CommitId, Error> {
        let entries = self.collect_tree_entries(&self.worktree_dir)?;
        let tree = gix_object::Tree { entries };
        let tree_oid = self.odb.write(&tree)?;

        let parent_oid = self.head_commit_oid()?;

        let signature = gix_actor::Signature {
            name: BString::from(SERVICE_AUTHOR_NAME),
            email: BString::from(SERVICE_AUTHOR_EMAIL),
            time: gix_date::Time::now_utc(),
        };

        let commit = gix_object::Commit {
            tree: tree_oid,
            parents: parent_oid.into_iter().collect(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: BString::from(message),
            extra_headers: vec![(
                BString::from(OPERATION_ID_HEADER),
                BString::from(operation_id),
            )],
        };
        let commit_oid = self.odb.write(&commit)?;

        self.update_branch(commit_oid, parent_oid, message)?;

        Ok(CommitId(commit_oid.to_string()))
    }

    /// The commit `refs/heads/main` currently points at, or `None` before
    /// the first commit. Exposed for tests and for the future kernel wiring
    /// to compare against without reaching into gitoxide types itself.
    pub fn head(&self) -> Result<Option<CommitId>, Error> {
        Ok(self.head_commit_oid()?.map(|oid| CommitId(oid.to_string())))
    }

    /// Read the parent-pointer chain from `refs/heads/main`, oldest first.
    /// Proves history accumulates rather than each commit silently replacing
    /// the last — the round-trip test's second half.
    pub fn history(&self) -> Result<Vec<CommitId>, Error> {
        let mut chain = Vec::new();
        let mut current = self.head_commit_oid()?;
        while let Some(oid) = current {
            let commit = self.read_commit(&oid)?;
            chain.push(CommitId(oid.to_string()));
            current = commit.parents.first().copied();
        }
        chain.reverse();
        Ok(chain)
    }

    /// The operation id a commit was made with, read back from its extra
    /// header — the other half of proving the id round-trips, not just the
    /// tree contents.
    pub fn operation_id(&self, commit: &CommitId) -> Result<Option<String>, Error> {
        let oid = commit.to_oid()?;
        let raw = self.read_commit(&oid)?;
        Ok(raw
            .extra_headers
            .iter()
            .find(|(key, _)| key == OPERATION_ID_HEADER.as_bytes())
            .map(|(_, value)| value.to_string()))
    }

    /// Read back the bytes of `rel_path` (worktree-relative, `/`-separated)
    /// as they were recorded in `commit`'s tree — never from the live
    /// worktree. This is what proves the commit actually captured file
    /// content rather than a name-only tree; a test comparing this against
    /// the live file only proves the walk found the file, not that its
    /// bytes made it into the object database.
    pub fn read_committed_file(&self, commit: &CommitId, rel_path: &str) -> Result<Vec<u8>, Error> {
        let commit_oid = commit.to_oid()?;
        let raw_commit = self.read_commit(&commit_oid)?;
        let mut tree_oid = raw_commit.tree;

        let mut components: Vec<&str> = rel_path.split('/').filter(|p| !p.is_empty()).collect();
        let file_name = components.pop().ok_or_else(|| Error::PathNotFound {
            commit: commit.as_str().to_string(),
            path: rel_path.to_string(),
        })?;

        for dir_name in &components {
            let mut buf = Vec::new();
            let data = self
                .odb
                .try_find(&tree_oid, &mut buf)?
                .ok_or_else(|| Error::CommitNotFound(tree_oid.to_string()))?;
            let tree = gix_object::TreeRef::from_bytes(data.data, gix_hash::Kind::Sha1)?;
            let entry = tree
                .entries
                .iter()
                .find(|e| e.filename == dir_name.as_bytes())
                .ok_or_else(|| Error::PathNotFound {
                    commit: commit.as_str().to_string(),
                    path: rel_path.to_string(),
                })?;
            tree_oid = entry.oid.into();
        }

        let mut buf = Vec::new();
        let data = self
            .odb
            .try_find(&tree_oid, &mut buf)?
            .ok_or_else(|| Error::CommitNotFound(tree_oid.to_string()))?;
        let tree = gix_object::TreeRef::from_bytes(data.data, gix_hash::Kind::Sha1)?;
        let entry = tree
            .entries
            .iter()
            .find(|e| e.filename == file_name.as_bytes())
            .ok_or_else(|| Error::PathNotFound {
                commit: commit.as_str().to_string(),
                path: rel_path.to_string(),
            })?;
        let blob_oid: gix_hash::ObjectId = entry.oid.into();

        let mut buf = Vec::new();
        let data = self
            .odb
            .try_find(&blob_oid, &mut buf)?
            .ok_or_else(|| Error::CommitNotFound(blob_oid.to_string()))?;
        Ok(data.data.to_vec())
    }

    fn read_commit(&self, oid: &gix_hash::ObjectId) -> Result<gix_object::Commit, Error> {
        let mut buf = Vec::new();
        let data = self
            .odb
            .try_find(oid, &mut buf)?
            .ok_or_else(|| Error::CommitNotFound(oid.to_string()))?;
        let commit_ref = gix_object::CommitRef::from_bytes(data.data, gix_hash::Kind::Sha1)?;
        Ok(commit_ref.into_owned()?)
    }

    fn head_commit_oid(&self) -> Result<Option<gix_hash::ObjectId>, Error> {
        let found = self.refs.try_find(self.branch.as_ref())?;
        Ok(found.and_then(|reference| match reference.target {
            Target::Object(oid) => Some(oid),
            // `main` is never made symbolic by this seam; a symbolic value
            // here means something outside this crate touched the ref, which
            // the "fail loud" policy says should surface, not be silently
            // treated as "no history yet". Treated as absent for now since
            // there's no kernel wiring yet to surface it *to* — the
            // kernel-wiring slice is where this becomes a hard error.
            Target::Symbolic(_) => None,
        }))
    }

    fn update_branch(
        &self,
        new_oid: gix_hash::ObjectId,
        previous_oid: Option<gix_hash::ObjectId>,
        message: &str,
    ) -> Result<(), Error> {
        let expected = match previous_oid {
            Some(oid) => PreviousValue::MustExistAndMatch(Target::Object(oid)),
            None => PreviousValue::MustNotExist,
        };
        // Reflog messages must be a single line (`gix_ref::transaction::LogChange`
        // doc comment); take the commit message's first line rather than
        // rejecting multi-line messages outright.
        let reflog_message = message.lines().next().unwrap_or_default();

        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: true,
                    message: BString::from(reflog_message),
                },
                expected,
                new: Target::Object(new_oid),
            },
            name: self.branch.clone(),
            deref: false,
        };

        let committer = gix_actor::Signature {
            name: BString::from(SERVICE_AUTHOR_NAME),
            email: BString::from(SERVICE_AUTHOR_EMAIL),
            time: gix_date::Time::now_utc(),
        };
        let mut time_buf = Default::default();

        self.refs
            .transaction()
            .prepare(
                vec![edit],
                gix_lock::acquire::Fail::Immediately,
                gix_lock::acquire::Fail::Immediately,
            )?
            .commit(Some(committer.to_ref(&mut time_buf)))?;

        Ok(())
    }

    /// Recursively build the sorted entry list for `dir`'s tree object,
    /// skipping `.git` at the worktree root and skipping any subdirectory
    /// that recurses to zero entries.
    ///
    /// The empty-directory skip isn't a shortcut, it's forced: git has no
    /// way to record an empty tree as a named entry inside a parent tree (a
    /// tree with zero entries can be *written*, as [`gix_object::Tree::empty`]
    /// documents, but nothing about the format lets a parent tree reference
    /// an empty child by name and have `git ls-tree` show a directory there —
    /// real git worktrees can't preserve empty directories either). This
    /// mirrors `docs/config-ownership.md`'s already-accepted limitation
    /// ("Rulings (Amy, 2026-08-15)", ruling 6: empty directories … git
    /// cannot track them either), not a new gap this crate introduces.
    fn collect_tree_entries(&self, dir: &Path) -> Result<Vec<TreeEntry>, Error> {
        let mut entries = Vec::new();
        let read_dir = fs::read_dir(dir).map_err(|e| Error::io("read_dir", dir, e))?;

        for dir_entry in read_dir {
            let dir_entry = dir_entry.map_err(|e| Error::io("read_dir entry", dir, e))?;
            let path = dir_entry.path();

            if dir == self.worktree_dir && dir_entry.file_name() == OsStr::new(".git") {
                continue;
            }

            let filename = os_str_to_bstring(&dir_entry.file_name(), &path)?;
            let file_type = dir_entry
                .file_type()
                .map_err(|e| Error::io("file_type", &path, e))?;

            if file_type.is_dir() {
                let sub_entries = self.collect_tree_entries(&path)?;
                if sub_entries.is_empty() {
                    continue;
                }
                let sub_tree = gix_object::Tree { entries: sub_entries };
                let oid = self.odb.write(&sub_tree)?;
                entries.push(TreeEntry {
                    mode: EntryKind::Tree.into(),
                    filename,
                    oid,
                });
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path).map_err(|e| Error::io("read_link", &path, e))?;
                let target_bytes = os_str_to_bstring(target.as_os_str(), &path)?;
                let oid = self
                    .odb
                    .write_buf(gix_object::Kind::Blob, target_bytes.as_slice())?;
                entries.push(TreeEntry {
                    mode: EntryKind::Link.into(),
                    filename,
                    oid,
                });
            } else if file_type.is_file() {
                let bytes = fs::read(&path).map_err(|e| Error::io("read", &path, e))?;
                let metadata = dir_entry.metadata().map_err(|e| Error::io("metadata", &path, e))?;
                let executable = metadata.permissions().mode() & 0o111 != 0;
                let kind = if executable {
                    EntryKind::BlobExecutable
                } else {
                    EntryKind::Blob
                };
                let oid = self.odb.write_buf(gix_object::Kind::Blob, &bytes)?;
                entries.push(TreeEntry {
                    mode: kind.into(),
                    filename,
                    oid,
                });
            }
            // Anything else (socket, fifo, device node) has no business in a
            // config worktree; silently skipping it would be exactly the
            // kind of silent fallback this project rejects, but there is no
            // reachable path to one today — nothing in rc/config/client/midi
            // creates such nodes. Left unhandled deliberately rather than
            // added speculatively; a future writer of one should get a loud
            // failure, which is filed as a gap rather than guessed at here.
        }

        entries.sort();
        Ok(entries)
    }
}

/// Convert a raw filename to the byte string git's tree format wants,
/// erroring instead of lossily substituting on the one input that can't
/// round-trip: an embedded NUL, which would truncate the entry in the
/// encoded tree and silently merge it with whatever follows.
fn os_str_to_bstring(os_str: &OsStr, context_path: &Path) -> Result<BString, Error> {
    let bytes = os_str.as_bytes();
    if bytes.contains(&0) {
        return Err(Error::InvalidEntryName(context_path.to_path_buf()));
    }
    Ok(BString::from(bytes))
}
