//! The statements of a shell case: each planned command as kaish planned
//! it, with what the kernel can resolve before anything runs
//! (`docs/council.md`, "The shell case").
//!
//! [`gather`] reads the facts that need the kernel: the mount table, how
//! each command name resolves, and which named paths exist. [`statements`]
//! is pure: it builds the case's `statements` from the plan and those facts.
//!
//! Resolution never runs anything. A command name resolves the way the
//! context shell resolves it: a tool the submission defines, then a kaijutsu
//! builtin, then a kaish builtin, then a host program on the context's
//! `PATH`, then a kaijutsu tool. Aliases and `.kai` scripts on `PATH` are
//! not resolved. An argument is shown as a path only when it names an
//! existing file or directory; the entry says where the word lands, not
//! that the command reads it as a path. A redirect target is always a path.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use kaish_kernel::PlannedStatement;
use kaish_types::plan::{PlannedCommand, PlannedValue};
use serde::Serialize;

use super::gate::elide;
use super::programs::{MOVERS, resolve};

/// Statement bytes shown in full before later statements are shortened.
const FULL_BUDGET: usize = 16 * 1024;
/// Statement bytes shown at all; later statements are counted, not shown.
const SHORT_BUDGET: usize = 20 * 1024;
/// Top-level commands shown per statement.
const MAX_COMMANDS: usize = 32;
/// Arguments shown per command.
const MAX_ARGS: usize = 48;
/// Bytes shown of one word, redirect target, or substitution.
const MAX_WORD: usize = 512;
/// Bytes shown of a heredoc body.
const MAX_HEREDOC: usize = 2048;
/// Bytes shown of a statement's rendering.
const MAX_RENDERED: usize = 4096;
/// Bytes shown of a shortened statement's rendering.
const SHORT_RENDERED: usize = 256;
/// Paths [`gather`] checks for existence, per submission.
const MAX_CHECKED: usize = 256;

/// Mounts the context shell adds over the kernel mount table, with whether
/// each is read-only, as `EmbeddedKaish` mounts them in a writable shell.
const SHELL_MOUNTS: [(&str, bool); 2] =
    [(kaijutsu_types::paths::DOCS_ROOT, false), (kaijutsu_types::paths::SWAP_ROOT, true)];

/// Why a relative path after a `cd` is not resolved.
const AFTER_CD: &str = "an earlier cd in this submission sets the directory this relative path lands in";

/// What the case says about mount access, once, when it names a mount.
pub(crate) const MOUNT_ACCESS: &str = "ro mounts refuse writes by kaish builtins and redirects; \
     rw mounts allow them. A host program is not limited by kaijutsu mounts.";

/// What a command name runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Runs {
    /// A tool an earlier `tool NAME { ... }` in the submission defines.
    Defined,
    /// A tool the context shell registers itself, such as `kj`.
    KaijutsuBuiltin,
    KaishBuiltin,
    /// A host program, with its path.
    Host(String),
    /// No builtin or kaijutsu tool has the name, and the context's loadout
    /// does not let it run host programs.
    HostRefused,
    /// A kaijutsu tool, run in the kernel.
    Tool,
    /// No builtin, host program, or kaijutsu tool has the name.
    NotFound,
    /// The name expands when the statement runs.
    Dynamic,
    /// The kernel could not read what resolves it; the cause.
    Unresolved(String),
}

impl Runs {
    fn words(&self) -> &'static str {
        match self {
            Self::Defined => "a tool this submission defines",
            Self::KaijutsuBuiltin => "kaijutsu builtin",
            Self::KaishBuiltin => "kaish builtin",
            Self::Host(_) => "host program",
            Self::HostRefused => {
                "nothing: no builtin or kaijutsu tool has this name, and this context may not run host programs; it fails"
            }
            Self::Tool => "kaijutsu tool",
            Self::NotFound => "nothing: no builtin, host program on PATH, or kaijutsu tool has this name; it fails",
            Self::Dynamic => "known only when it runs",
            Self::Unresolved(_) => "not resolved",
        }
    }
}

/// What the kernel knows about a submission before it runs.
#[derive(Clone, Debug, Default)]
pub(crate) struct Facts {
    /// The directory relative paths resolve against.
    pub(crate) cwd: String,
    /// Mount points, each with whether it is read-only.
    pub(crate) mounts: Vec<(String, bool)>,
    /// What each command name runs.
    pub(crate) names: HashMap<String, Runs>,
    /// The checked absolute paths that exist.
    pub(crate) existing: HashSet<String>,
}

impl Facts {
    /// The mount `path` lands on, by longest prefix, and whether it is
    /// read-only.
    fn mount_of(&self, path: &str) -> Option<(&str, bool)> {
        self.mounts
            .iter()
            .filter(|(point, _)| {
                point == "/" || path == point || path.strip_prefix(point.as_str()).is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(point, _)| point.len())
            .map(|(point, read_only)| (point.as_str(), *read_only))
    }

    fn runs(&self, name: &str) -> Runs {
        self.names.get(name).cloned().unwrap_or_else(|| Runs::Unresolved("the name was not looked up".into()))
    }
}

fn kaish_builtins() -> &'static HashSet<String> {
    static NAMES: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut registry = kaish_kernel::ToolRegistry::new();
        kaish_kernel::tools::register_builtins(&mut registry);
        registry.names().into_iter().map(str::to_string).collect()
    })
}

/// Names the submission defines with a top-level `tool NAME { ... }`.
fn defined_tools(command: &str) -> HashSet<String> {
    let Ok(program) = kaish_kernel::parser::parse(command) else { return HashSet::new() };
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            kaish_kernel::ast::Stmt::ToolDef(def) => Some(def.name.clone()),
            _ => None,
        })
        .collect()
}

/// Every command in the plan, substitutions included.
fn all_commands(planned: &[PlannedStatement]) -> impl Iterator<Item = &PlannedCommand> {
    planned.iter().flat_map(|s| s.plan.commands.iter())
}

/// The literal words of `command` that may name a path: arguments that are
/// not flags, and file redirect targets.
fn path_words(command: &PlannedCommand) -> impl Iterator<Item = &str> {
    let args = command.args.iter().filter_map(PlannedValue::literal_value).filter(|w| !w.starts_with('-'));
    let targets = command.redirects.iter().filter(|r| is_file_redirect(&r.kind)).filter_map(|r| r.target.literal_value());
    args.chain(targets).filter(|w| !w.is_empty() && w.len() <= 4096 && !w.contains('\n'))
}

/// Read what the kernel knows about `planned` before it runs, for the
/// context `caller` submits from, with relative paths resolved against
/// `cwd`.
pub(crate) async fn gather(
    kernel: &crate::Kernel,
    caller: &crate::kj::KjCaller,
    context: kaijutsu_types::ContextId,
    command: &str,
    planned: &[PlannedStatement],
    cwd: &str,
) -> Facts {
    use crate::runtime::context_shell::{CONTEXT_SHELL_BUILTINS, ContextShellInputs, ShellCwd};
    use crate::vfs::VfsOps;

    let mut mounts: Vec<(String, bool)> =
        kernel.vfs().list_mounts().await.into_iter().map(|m| (m.path.to_string_lossy().into_owned(), m.read_only)).collect();
    mounts.extend(SHELL_MOUNTS.iter().map(|(point, read_only)| (point.to_string(), *read_only)));

    let shell = ContextShellInputs::load(kernel, context, false, ShellCwd::Context).await;
    let (exec, path) = match &shell {
        Ok(inputs) => (
            Ok(matches!(inputs.external_exec, crate::runtime::embedded_kaish::ExternalExec::Allow { .. })),
            inputs.environment().get("PATH").cloned().unwrap_or_default(),
        ),
        Err(e) => (Err(format!("the context shell's settings could not be read: {e}")), String::new()),
    };
    let defined = defined_tools(command);
    let mut tools: Option<HashSet<String>> = None;
    let mut names = HashMap::new();
    for name in all_commands(planned).map(|c| c.name.as_str()) {
        if names.contains_key(name) {
            continue;
        }
        let builtin = name.strip_prefix("/v/bin/").unwrap_or(name);
        let runs = if name.contains('$') {
            Runs::Dynamic
        } else if defined.contains(name) {
            Runs::Defined
        } else if CONTEXT_SHELL_BUILTINS.contains(&builtin) {
            Runs::KaijutsuBuiltin
        } else if kaish_builtins().contains(builtin) {
            Runs::KaishBuiltin
        } else {
            match &exec {
                Err(cause) => Runs::Unresolved(cause.clone()),
                Ok(exec) => {
                    let host = if !exec {
                        None
                    } else if name.contains('/') {
                        let resolved = resolve(cwd, name);
                        kernel.vfs().exists(Path::new(&resolved)).await.then_some(resolved)
                    } else {
                        kaish_kernel::tools::resolve_in_path(name, &path)
                    };
                    match host {
                        Some(found) => Runs::Host(found),
                        None => {
                            if tools.is_none() {
                                tools = Some(visible_tools(kernel, caller, context).await);
                            }
                            if tools.as_ref().is_some_and(|t| t.contains(name)) {
                                Runs::Tool
                            } else if *exec || name.contains('/') {
                                Runs::NotFound
                            } else {
                                Runs::HostRefused
                            }
                        }
                    }
                }
            }
        };
        names.insert(name.to_string(), runs);
    }

    let mut candidates: Vec<String> = Vec::new();
    for word in all_commands(planned).flat_map(path_words) {
        let path = resolve(cwd, word);
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    if candidates.len() > MAX_CHECKED {
        tracing::debug!(paths = candidates.len(), "the council checks only the first {MAX_CHECKED} paths a submission names");
        candidates.truncate(MAX_CHECKED);
    }
    let mut existing = HashSet::new();
    for path in candidates {
        if kernel.vfs().exists(Path::new(&path)).await {
            existing.insert(path);
        }
    }
    Facts { cwd: cwd.to_string(), mounts, names, existing }
}

/// The kaijutsu tools the context's commands can call; empty when the
/// broker cannot list them.
async fn visible_tools(kernel: &crate::Kernel, caller: &crate::kj::KjCaller, context: kaijutsu_types::ContextId) -> HashSet<String> {
    let call = crate::mcp::CallContext::new(caller.principal_id, context, caller.session_id, kernel.id())
        .with_actor(caller.actor_id, caller.reviewer_id);
    match kernel.broker().list_visible_tools(context, &call).await {
        Ok(tools) => tools.into_iter().map(|(name, _)| name).collect(),
        Err(e) => {
            tracing::debug!("the council could not list the context's kaijutsu tools: {e}");
            HashSet::new()
        }
    }
}

/// `text` cut to `limit` bytes, with a marker naming what was cut.
pub(crate) fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}[... {} more bytes not shown]", &text[..cut], text.len() - cut)
}

fn is_file_redirect(kind: &str) -> bool {
    matches!(kind, ">" | ">>" | "2>" | "&>" | "<")
}

/// One argument: the word the command receives, or the text that expands
/// when the statement runs.
#[derive(Serialize)]
#[serde(untagged)]
enum Word {
    Known(String),
    Unexpanded { unexpanded: String },
}

#[derive(Serialize)]
struct PathView {
    word: String,
    path: String,
    mount: String,
    access: &'static str,
}

#[derive(Serialize)]
struct RedirectView {
    op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    writes: Option<Word>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reads: Option<Word>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    access: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

#[derive(Serialize)]
struct HeredocView {
    delimiter: String,
    /// Whether `${...}` and `$(...)` in the body expand before the command
    /// reads it.
    expands: bool,
    body: String,
}

#[derive(Serialize)]
struct SubstitutionView {
    text: String,
    commands: Vec<CommandView>,
}

#[derive(Serialize)]
struct CommandView {
    name: String,
    runs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<Word>,
    #[serde(skip_serializing_if = "is_zero")]
    args_not_shown: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<PathView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redirects: Vec<RedirectView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    heredocs: Vec<HeredocView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    substitutions: Vec<SubstitutionView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    background: bool,
}

/// One statement of the shell case.
#[derive(Serialize)]
pub(crate) struct StatementView {
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendered: Option<String>,
    kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    commands: Vec<CommandView>,
    #[serde(skip_serializing_if = "is_zero")]
    commands_not_shown: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reads_variables: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sets_variables: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The statements as the case shows them, and how many it leaves out.
pub(crate) struct Statements {
    pub(crate) shown: Vec<StatementView>,
    pub(crate) not_shown: usize,
    /// Whether any statement names a mount.
    pub(crate) names_a_mount: bool,
}

/// A command and the commands its substitutions run, in plan order.
struct Node<'a> {
    /// The command's position in its statement's plan.
    index: usize,
    command: &'a PlannedCommand,
    substitutions: Vec<(String, Vec<Node<'a>>)>,
}

/// The `$(...)` texts of `command`'s arguments and file redirect targets,
/// in the order kaish plans their commands.
fn substitution_texts(command: &PlannedCommand) -> Vec<&str> {
    let args = command.args.iter();
    let targets = command.redirects.iter().filter(|r| r.kind != "<<").map(|r| &r.target);
    args.chain(targets)
        .filter_map(|v| match v {
            PlannedValue::Plain(text) if text.contains("$(") => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The commands the substitutions in `text` run, as kaish plans them;
/// `None` when the text does not plan as one word.
fn substitution_commands(text: &str) -> Option<Vec<PlannedCommand>> {
    let planned = kaish_kernel::plan_program(&format!("echo {text}")).ok()?;
    let [statement] = planned.as_slice() else { return None };
    let (carrier, inner) = statement.plan.commands.split_first()?;
    (carrier.name == "echo").then(|| inner.to_vec())
}

/// Group `flat`, one statement's planned commands, under the substitutions
/// that run them. kaish plans a command's substitution commands right after
/// it; a command whose substitutions do not plan to the commands that
/// follow it keeps them as its siblings.
fn nest(flat: &[PlannedCommand], offset: usize) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < flat.len() {
        let command = &flat[i];
        let mut next = i + 1;
        let mut substitutions = Vec::new();
        for text in substitution_texts(command) {
            let inner = substitution_commands(text);
            let end = next + inner.as_ref().map_or(0, Vec::len);
            let matches = inner.as_ref().is_some_and(|inner| {
                end <= flat.len() && flat[next..end].iter().zip(inner).all(|(a, b)| a.name == b.name && a.args == b.args)
            });
            if !matches {
                substitutions.clear();
                next = i + 1;
                break;
            }
            if end > next {
                substitutions.push((text.to_string(), nest(&flat[next..end], offset + next)));
            }
            next = end;
        }
        out.push(Node { index: offset + i, command, substitutions });
        i = next;
    }
    out
}

struct Builder<'a> {
    facts: &'a Facts,
    judged: &'a [&'a str],
    names_a_mount: bool,
}

impl Builder<'_> {
    fn elide(&self, text: &str) -> String {
        elide(text, self.judged)
    }

    fn word(&self, value: &PlannedValue) -> Word {
        match value.literal_value() {
            Some(word) => Word::Known(clip(&self.elide(word), MAX_WORD)),
            None => Word::Unexpanded { unexpanded: clip(&self.elide(&value.display()), MAX_WORD) },
        }
    }

    fn access(&mut self, path: &str) -> Option<(String, &'static str)> {
        let (point, read_only) = self.facts.mount_of(path)?;
        self.names_a_mount = true;
        Some((point.to_string(), if read_only { "ro" } else { "rw" }))
    }

    /// `word` as a path, when it names an existing file or directory.
    fn path(&mut self, word: &str, moved: bool) -> Option<PathView> {
        if word.starts_with('-') || (moved && !word.starts_with('/')) || self.elide(word) != word {
            return None;
        }
        let path = resolve(&self.facts.cwd, word);
        if !self.facts.existing.contains(&path) {
            return None;
        }
        let (mount, access) = self.access(&path)?;
        Some(PathView { word: clip(word, MAX_WORD), path: clip(&path, MAX_WORD), mount, access })
    }

    fn redirect(&mut self, op: &str, target: &PlannedValue, moved: bool) -> RedirectView {
        let mut view = RedirectView {
            op: op.to_string(),
            writes: None,
            reads: None,
            text: None,
            exists: None,
            mount: None,
            access: None,
            note: None,
        };
        if !is_file_redirect(op) {
            if op == "<<<" {
                view.text = Some(clip(&self.elide(&target.display()), MAX_WORD));
            }
            return view;
        }
        let shown = match target.literal_value() {
            Some(word) if moved && !word.starts_with('/') => {
                view.note = Some(AFTER_CD);
                Word::Known(clip(word, MAX_WORD))
            }
            Some(word) => {
                let path = resolve(&self.facts.cwd, word);
                view.exists = Some(self.facts.existing.contains(&path));
                if let Some((mount, access)) = self.access(&path) {
                    view.mount = Some(mount);
                    view.access = Some(access);
                }
                Word::Known(clip(&path, MAX_WORD))
            }
            None => self.word(target),
        };
        if op == "<" {
            view.reads = Some(shown);
        } else {
            view.writes = Some(shown);
        }
        view
    }

    fn command(&mut self, node: &Node<'_>, moved: &[bool]) -> CommandView {
        let command = node.command;
        let moved_here = moved[node.index];
        let runs = self.facts.runs(&command.name);
        let relative_after_cd = moved_here && !command.name.starts_with('/') && command.name.contains('/');
        let (program, note) = match &runs {
            Runs::Host(_) if relative_after_cd => (None, Some(AFTER_CD.to_string())),
            Runs::Host(path) => (Some(path.clone()), None),
            Runs::Unresolved(cause) => (None, Some(cause.clone())),
            _ => (None, None),
        };
        let args: Vec<Word> = command.args.iter().take(MAX_ARGS).map(|a| self.word(a)).collect();
        let paths: Vec<PathView> = command
            .args
            .iter()
            .take(MAX_ARGS)
            .filter_map(PlannedValue::literal_value)
            .filter_map(|w| self.path(w, moved_here))
            .collect();
        let redirects =
            command.redirects.iter().filter(|r| r.kind != "<<").map(|r| self.redirect(&r.kind, &r.target, moved_here)).collect();
        let heredocs = command
            .heredocs
            .iter()
            .map(|h| HeredocView {
                delimiter: h.delimiter.clone(),
                expands: !h.literal,
                body: clip(&self.elide(&h.body.display()), MAX_HEREDOC),
            })
            .collect();
        let substitutions = node
            .substitutions
            .iter()
            .map(|(text, inner)| SubstitutionView {
                text: clip(&self.elide(text), MAX_WORD),
                commands: inner.iter().map(|n| self.command(n, moved)).collect(),
            })
            .collect();
        CommandView {
            name: clip(&command.name, MAX_WORD),
            runs: runs.words(),
            program,
            note,
            args,
            args_not_shown: command.args.len().saturating_sub(MAX_ARGS),
            paths,
            redirects,
            heredocs,
            substitutions,
            background: command.background,
        }
    }
}

/// The case's statements, in plan order. Each of `judged` is shown as
/// `<program judged separately>`. A statement's `rendered` is left out when it is
/// the whole submission, `command`. Statements past [`FULL_BUDGET`] bytes
/// show only their kind and a short rendering, and statements past
/// [`SHORT_BUDGET`] are counted in `not_shown`.
pub(crate) fn statements(command: &str, planned: &[PlannedStatement], facts: &Facts, judged: &[&str]) -> Statements {
    let mut builder = Builder { facts, judged, names_a_mount: false };
    let mut shown = Vec::new();
    let mut not_shown = 0;
    let mut used = 0;
    let mut moved = false;
    for statement in planned {
        let flat = &statement.plan.commands;
        // `moved_before[i]`: a command before command `i` changes the
        // working directory.
        let mut moved_before = Vec::with_capacity(flat.len());
        for c in flat {
            moved_before.push(moved);
            moved |= MOVERS.contains(&c.name.rsplit('/').next().unwrap_or(&c.name));
        }
        if used >= SHORT_BUDGET {
            not_shown += 1;
            continue;
        }
        let rendered = builder.elide(&statement.plan.rendered);
        let whole = planned.len() == 1 && rendered.trim() == builder.elide(command).trim();
        let nodes = nest(flat, 0);
        let mounts_before = builder.names_a_mount;
        let full = StatementView {
            index: statement.index,
            rendered: (!whole).then(|| clip(&rendered, MAX_RENDERED)),
            kind: statement.plan.statement_kind.clone(),
            commands: nodes.iter().take(MAX_COMMANDS).map(|n| builder.command(n, &moved_before)).collect(),
            commands_not_shown: nodes.len().saturating_sub(MAX_COMMANDS),
            reads_variables: statement.plan.free_variables.clone(),
            sets_variables: statement.plan.bound_variables.clone(),
        };
        let size = serde_json::to_string(&full).map(|s| s.len()).unwrap_or(usize::MAX);
        if used + size <= FULL_BUDGET {
            used += size;
            shown.push(full);
            continue;
        }
        builder.names_a_mount = mounts_before;
        let short = StatementView {
            index: statement.index,
            rendered: Some(clip(&rendered, SHORT_RENDERED)),
            kind: statement.plan.statement_kind.clone(),
            commands: Vec::new(),
            commands_not_shown: nodes.len(),
            reads_variables: Vec::new(),
            sets_variables: Vec::new(),
        };
        used += serde_json::to_string(&short).map(|s| s.len()).unwrap_or(SHORT_BUDGET);
        shown.push(short);
    }
    Statements { shown, not_shown, names_a_mount: builder.names_a_mount }
}

#[cfg(test)]
impl Facts {
    /// Facts for a test: `mounts` as `(point, read_only)`, `hosts` as host
    /// program names with their paths, and `existing` absolute paths. Every
    /// other command name resolves as a context shell resolves it with no
    /// kaijutsu tools: a builtin, or nothing.
    pub(crate) fn for_test(
        cwd: &str,
        mounts: &[(&str, bool)],
        hosts: &[(&str, &str)],
        existing: &[&str],
        planned: &[PlannedStatement],
    ) -> Self {
        use crate::runtime::context_shell::CONTEXT_SHELL_BUILTINS;
        let names = all_commands(planned)
            .map(|c| {
                let name = c.name.as_str();
                let runs = match hosts.iter().find(|(n, _)| *n == name) {
                    Some((_, path)) => Runs::Host(path.to_string()),
                    None if CONTEXT_SHELL_BUILTINS.contains(&name) => Runs::KaijutsuBuiltin,
                    None if kaish_builtins().contains(name) => Runs::KaishBuiltin,
                    None => Runs::NotFound,
                };
                (name.to_string(), runs)
            })
            .collect();
        Facts {
            cwd: cwd.to_string(),
            mounts: mounts.iter().map(|(p, ro)| (p.to_string(), *ro)).collect(),
            names,
            existing: existing.iter().map(|p| p.to_string()).collect(),
        }
    }
}
