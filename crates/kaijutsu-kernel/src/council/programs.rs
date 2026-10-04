//! The programs a shell submission runs, found from its plan before it runs
//! (`docs/council.md`, "Programs are cases of their own").
//!
//! [`programs_in`] is pure: it reads the planned statements and the seat's
//! working directory, and names each program by where its text comes from.
//! A program whose text is not known before the statement runs is
//! [`ProgramSource::Unknown`], with the reason in plain words; the gate never
//! counts one as judged. Reading files is [`read_programs`]' job.
//!
//! Recognized forms:
//!
//! - python (`python3`, `python`, `python3.12`, a path ending in one): a
//!   script operand, `-c CODE`, a heredoc or `<` file on stdin, and `-m NAME`
//!   when `NAME` resolves to a file beside the working directory;
//! - `bash`, `sh`, `dash`, `zsh`, `ksh`: a script operand, `-c CODE`, or a
//!   heredoc on stdin;
//! - a command named by a path (`./fix.sh`): a program when the file starts
//!   with `#!`;
//! - each of those behind `env`, `timeout`, `nice`, `nohup`, or `time`.
//!
//! A file the submission writes before it runs it is judged by the text the
//! submission writes when that is a `cat > FILE` heredoc, and is unknown
//! otherwise. A relative path after a `cd` in the same submission is
//! unknown.

use std::path::{Component, Path, PathBuf};

use kaish_kernel::PlannedStatement;
use kaish_types::plan::{PlannedCommand, PlannedValue};

use crate::runtime::python_tool::{python_argv, Program};

/// The language a program is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    Python,
    /// A POSIX-family shell script.
    Shell,
    /// A file run by its own `#!` line.
    Shebang,
}

impl Language {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Shell => "shell",
            Self::Shebang => "shebang",
        }
    }
}

/// Where a program's text comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProgramSource {
    /// A file, as an absolute path. `shebang` means the file is a program
    /// only when it starts with `#!`.
    File { path: String, shebang: bool },
    /// Text the submission carries: `-c CODE`, or a heredoc.
    Text(String),
    /// Text the submission writes to `path` with a `cat > FILE` heredoc
    /// before it runs that file.
    Written { path: String, text: String },
    /// `python3 -m NAME`: the files NAME would load from beside the working
    /// directory, in the order python tries them. None existing means
    /// installed library code, which is not a program of the submission's.
    Module { name: String, candidates: Vec<String> },
    /// The text is not known before the statement runs.
    Unknown(String),
}

/// One program a submission runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgramRun {
    /// The planned statement it runs in.
    pub(crate) statement: usize,
    /// The command that runs it, as written.
    pub(crate) command: String,
    pub(crate) language: Language,
    pub(crate) source: ProgramSource,
}

/// Shells whose script operand, `-c`, and stdin forms are read here.
const SHELLS: [&str; 5] = ["bash", "sh", "dash", "zsh", "ksh"];

/// Commands that run the command after their own options.
const WRAPPERS: [&str; 5] = ["env", "timeout", "nice", "nohup", "time"];

/// Commands that name a file without writing it.
const READERS: [&str; 18] = [
    "cat", "chmod", "ls", "head", "tail", "wc", "file", "stat", "test", "[", "grep", "rg", "diff", "sha256sum",
    "md5sum", "less", "more", "echo",
];

/// Commands that change the working directory.
const MOVERS: [&str; 3] = ["cd", "pushd", "popd"];

fn base_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The command as written, for the case state and messages.
fn rendered(command: &PlannedCommand) -> String {
    let mut words = vec![command.name.clone()];
    words.extend(command.args.iter().map(PlannedValue::display));
    words.join(" ")
}

/// `path` resolved lexically against `cwd`, with `.` and `..` removed.
pub(crate) fn resolve(cwd: &str, path: &str) -> String {
    let joined = if path.starts_with('/') { PathBuf::from(path) } else { Path::new(cwd).join(path) };
    let mut out = PathBuf::from("/");
    for part in joined.components() {
        match part {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out.to_string_lossy().into_owned()
}

/// Heredoc text as the command reads it, when no expansion can change it:
/// a quoted delimiter, or a body with no `$`, backquote, or backslash.
fn heredoc_text(command: &PlannedCommand) -> Option<String> {
    let heredoc = command.heredocs.first()?;
    let body = match &heredoc.body {
        PlannedValue::Plain(text) => text.as_str(),
        PlannedValue::Literal { value, .. } => value.as_str(),
        _ => return None,
    };
    if !heredoc.literal && body.contains(['$', '`', '\\']) {
        return None;
    }
    Some(if heredoc.strip_tabs {
        body.split_inclusive('\n').map(|line| line.trim_start_matches('\t')).collect()
    } else {
        body.to_string()
    })
}

/// The literal target of a `<` redirect, when the command has one.
fn stdin_file(command: &PlannedCommand) -> Option<Option<&str>> {
    command.redirects.iter().rev().find(|r| r.kind == "<").map(|r| r.target.literal_value())
}

/// The command a wrapper runs, after the wrapper's own words. `Err` when
/// that command's name expands at run time.
fn unwrap_command(command: &PlannedCommand) -> Option<Result<PlannedCommand, String>> {
    let wrapper = base_name(&command.name);
    if !WRAPPERS.contains(&wrapper) {
        return None;
    }
    let args = &command.args;
    let mut index = 0;
    let mut positional_left = usize::from(wrapper == "timeout");
    while let Some(arg) = args.get(index) {
        let Some(word) = arg.literal_value() else {
            return Some(Err(format!("`{}` runs a command named at run time", rendered(command))));
        };
        if word == "--" {
            index += 1;
            break;
        }
        if word.starts_with('-') && word.len() > 1 {
            // `nice -n 5`, `timeout -s KILL`, `timeout -k 5`, `env -u NAME`.
            let takes_value = matches!((wrapper, word), ("nice", "-n") | ("timeout", "-s" | "-k") | ("env", "-u" | "-C"));
            index += 1 + usize::from(takes_value);
            continue;
        }
        if wrapper == "env" && word.contains('=') {
            index += 1;
            continue;
        }
        if positional_left > 0 {
            positional_left -= 1;
            index += 1;
            continue;
        }
        break;
    }
    let name = args.get(index)?;
    let Some(name) = name.literal_value() else {
        return Some(Err(format!("`{}` runs a command named at run time", rendered(command))));
    };
    let inner = PlannedCommand::new(name, args[index + 1..].to_vec(), command.redirects.clone(), command.background)
        .with_heredocs(command.heredocs.clone());
    Some(Ok(inner))
}

/// What a shell command line runs: `Ok(None)` when it reads stdin.
enum ShellProgram {
    Script(PlannedValue),
    Inline(PlannedValue),
    Stdin,
    Unknown,
}

fn shell_argv(command: &PlannedCommand) -> ShellProgram {
    let args = &command.args;
    let mut index = 0;
    let mut inline = false;
    let mut stdin = false;
    while let Some(arg) = args.get(index) {
        let Some(word) = arg.literal_value() else { return ShellProgram::Unknown };
        index += 1;
        if word == "--" || word == "-" {
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            if matches!(long, "rcfile" | "init-file") {
                index += 1;
            }
            continue;
        }
        let Some(cluster) = word.strip_prefix('-').or_else(|| word.strip_prefix('+')) else {
            index -= 1;
            break;
        };
        for letter in cluster.chars() {
            match letter {
                'c' => inline = true,
                's' => stdin = true,
                'o' | 'O' => index += 1,
                _ => {}
            }
        }
    }
    match args.get(index) {
        Some(word) if inline => ShellProgram::Inline(word.clone()),
        None if inline => ShellProgram::Unknown,
        Some(_) if stdin => ShellProgram::Stdin,
        Some(word) => ShellProgram::Script(word.clone()),
        None => ShellProgram::Stdin,
    }
}

/// A path operand, resolved, or why it cannot be.
fn path_source(word: &PlannedValue, cwd: &str, moved: bool, command: &PlannedCommand, shebang: bool) -> ProgramSource {
    let Some(path) = word.literal_value() else {
        return ProgramSource::Unknown(format!("`{}` names its program with a word that expands at run time", rendered(command)));
    };
    if moved && !path.starts_with('/') {
        return ProgramSource::Unknown(format!(
            "the submission changes directory before `{}` runs, so the kernel cannot tell which file `{path}` names",
            rendered(command)
        ));
    }
    ProgramSource::File { path: resolve(cwd, path), shebang }
}

fn stdin_source(command: &PlannedCommand, cwd: &str, moved: bool) -> ProgramSource {
    if let Some(text) = heredoc_text(command) {
        return ProgramSource::Text(text);
    }
    if !command.heredocs.is_empty() {
        return ProgramSource::Unknown(format!(
            "`{}` reads a heredoc that expands at run time",
            rendered(command)
        ));
    }
    match stdin_file(command) {
        Some(Some(path)) => path_source(&PlannedValue::literal(path, path), cwd, moved, command, false),
        Some(None) => ProgramSource::Unknown(format!("`{}` reads a file named at run time", rendered(command))),
        None => ProgramSource::Unknown(format!(
            "`{}` reads its program from standard input, which the submission does not show",
            rendered(command)
        )),
    }
}

/// The program `command` runs, when it runs one.
fn program_of(command: &PlannedCommand, cwd: &str, moved: bool) -> Option<(Language, ProgramSource)> {
    if let Some(inner) = unwrap_command(command) {
        return match inner {
            Ok(inner) => program_of(&inner, cwd, moved),
            Err(why) => Some((Language::Shebang, ProgramSource::Unknown(why))),
        };
    }
    if command.name.contains(['$', '`']) {
        return Some((
            Language::Shebang,
            ProgramSource::Unknown(format!("`{}` runs a command named at run time", rendered(command))),
        ));
    }
    if let Some(argv) = python_argv(command) {
        if argv.informational {
            return None;
        }
        let source = match argv.program {
            Program::Script(word) => path_source(&word, cwd, moved, command, false),
            Program::Inline(code) => match code.literal_value() {
                Some(text) => ProgramSource::Text(text.to_string()),
                None => ProgramSource::Unknown(format!("`{}` passes code that expands at run time", rendered(command))),
            },
            Program::Stdin => stdin_source(command, cwd, moved),
            Program::Module(module) => match module.literal_value() {
                Some(_) if moved => ProgramSource::Unknown(format!(
                    "the submission changes directory before `{}` runs, so the kernel cannot tell which module it loads",
                    rendered(command)
                )),
                Some(name) => {
                    let stem = name.replace('.', "/");
                    ProgramSource::Module {
                        name: name.to_string(),
                        candidates: vec![resolve(cwd, &format!("{stem}.py")), resolve(cwd, &format!("{stem}/__main__.py"))],
                    }
                }
                None => ProgramSource::Unknown(format!("`{}` names a module at run time", rendered(command))),
            },
            Program::Unknown => ProgramSource::Unknown(format!(
                "`{}` names its program with a word that expands at run time",
                rendered(command)
            )),
        };
        return Some((Language::Python, source));
    }
    if SHELLS.contains(&base_name(&command.name)) {
        let source = match shell_argv(command) {
            ShellProgram::Script(word) => path_source(&word, cwd, moved, command, false),
            ShellProgram::Inline(code) => match code.literal_value() {
                Some(text) => ProgramSource::Text(text.to_string()),
                None => ProgramSource::Unknown(format!("`{}` passes code that expands at run time", rendered(command))),
            },
            ShellProgram::Stdin => stdin_source(command, cwd, moved),
            ShellProgram::Unknown => ProgramSource::Unknown(format!(
                "`{}` names its program with a word that expands at run time",
                rendered(command)
            )),
        };
        return Some((Language::Shell, source));
    }
    if command.name.contains('/') {
        let word = PlannedValue::literal(command.name.clone(), command.name.clone());
        return Some((Language::Shebang, path_source(&word, cwd, moved, command, true)));
    }
    None
}

/// A write to a file before a program runs, as the program's file sees it.
enum Write {
    /// `cat > FILE` from a heredoc whose text is known.
    Heredoc(String),
    /// Anything else that may change the file.
    Other(String),
}

/// The writes `command` makes, each with the absolute path it writes, or
/// with the bare file name when a `cd` came first and the path is relative.
/// A path that is the file `command` itself runs (`own`) is read, not
/// written, unless a redirect writes it.
fn writes_of(command: &PlannedCommand, cwd: &str, moved: bool, own: Option<&str>) -> Vec<(String, bool, Write)> {
    let mut out = Vec::new();
    let place = |path: &str| -> (String, bool) {
        if moved && !path.starts_with('/') {
            (base_name(path).to_string(), true)
        } else {
            (resolve(cwd, path), false)
        }
    };
    let name = base_name(&command.name);
    for redirect in &command.redirects {
        if !redirect.kind.contains('>') {
            continue;
        }
        let Some(target) = redirect.target.literal_value() else { continue };
        let (path, bare) = place(target);
        let write = match heredoc_text(command) {
            Some(text) if name == "cat" && redirect.kind == ">" && command.args.is_empty() => Write::Heredoc(text),
            _ => Write::Other(rendered(command)),
        };
        out.push((path, bare, write));
    }
    if !READERS.contains(&name) {
        for arg in &command.args {
            let Some(word) = arg.literal_value() else { continue };
            if word.starts_with('-') {
                continue;
            }
            let (path, bare) = place(word);
            if !bare && Some(path.as_str()) == own {
                continue;
            }
            out.push((path, bare, Write::Other(rendered(command))));
        }
    }
    out
}

/// Every program the planned submission runs, in plan order, with paths
/// resolved against `cwd`.
pub(crate) fn programs_in(planned: &[PlannedStatement], cwd: &str) -> Vec<ProgramRun> {
    let mut programs = Vec::new();
    let mut moved = false;
    let mut written: Vec<(String, bool, Write)> = Vec::new();
    for statement in planned {
        for command in &statement.plan.commands {
            let program = program_of(command, cwd, moved);
            let own = match &program {
                Some((_, ProgramSource::File { path, .. })) => Some(path.clone()),
                _ => None,
            };
            let writes = writes_of(command, cwd, moved, own.as_deref());
            if let Some((language, mut source)) = program {
                if let ProgramSource::File { path, .. } = &source {
                    let name = base_name(path);
                    let before: Vec<_> = written
                        .iter()
                        .chain(writes.iter().filter(|(_, _, w)| matches!(w, Write::Other(_))))
                        .filter(|(p, bare, _)| if *bare { p == name } else { p == path })
                        .collect();
                    // Only a heredoc written to this exact path, with no
                    // other write to it, says what runs.
                    let heredoc = match before.as_slice() {
                        [] => None,
                        [.., (_, false, Write::Heredoc(text))]
                            if before.iter().all(|(_, bare, w)| !*bare && matches!(w, Write::Heredoc(_))) =>
                        {
                            Some(Ok(text.clone()))
                        }
                        _ => Some(Err(())),
                    };
                    source = match heredoc {
                        None => source,
                        Some(Ok(text)) => ProgramSource::Written { path: path.clone(), text },
                        Some(Err(())) => ProgramSource::Unknown(format!(
                            "the submission writes {path} before `{}` runs it",
                            rendered(command)
                        )),
                    };
                }
                programs.push(ProgramRun { statement: statement.index, command: rendered(command), language, source });
            }
            written.extend(writes);
            if MOVERS.contains(&base_name(&command.name)) {
                moved = true;
            }
        }
    }
    programs
}

/// The most program text a decision reads: 16 KiB. A larger file is not
/// judged.
pub(crate) const PROGRAM_CAP: usize = 16 * 1024;

/// A file the council judged, by the hash of the exact bytes it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JudgedFile {
    pub(crate) path: String,
    /// `sha256:` and the hex digest of the file's bytes.
    pub(crate) sha256: String,
}

/// A program with its text, or the reason the text could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadProgram {
    pub(crate) run: ProgramRun,
    pub(crate) text: Result<ProgramText, String>,
}

/// The text a program decision reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgramText {
    pub(crate) text: String,
    /// The file the text was read from, when it was read from one.
    pub(crate) file: Option<JudgedFile>,
    /// The file the submission writes this text to before running it.
    pub(crate) written: Option<String>,
    /// Top-level python imports that name a file beside the program; the
    /// decision does not show their text.
    pub(crate) imports_not_shown: Vec<String>,
}

pub(crate) fn sha256_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Read `path` whole, refusing more than [`PROGRAM_CAP`] bytes.
async fn read_capped<V: crate::vfs::VfsOps + ?Sized>(vfs: &V, path: &str) -> Result<Vec<u8>, String> {
    let at = Path::new(path);
    let attr = vfs.getattr(at).await.map_err(|e| format!("{path} could not be read: {e}"))?;
    if !attr.kind.is_file() {
        return Err(format!("{path} is not a regular file"));
    }
    if attr.size as usize > PROGRAM_CAP {
        return Err(format!("{path} is {} bytes, more than the {PROGRAM_CAP}-byte limit a decision reads", attr.size));
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = vfs
            .read(at, bytes.len() as u64, (PROGRAM_CAP + 1 - bytes.len()) as u32)
            .await
            .map_err(|e| format!("{path} could not be read: {e}"))?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() > PROGRAM_CAP {
            return Err(format!("{path} is more than the {PROGRAM_CAP}-byte limit a decision reads"));
        }
    }
    Ok(bytes)
}

/// The first dotted name of each top-level `import` and `from ... import`
/// line, in order, without repeats. Relative imports are left out.
fn top_level_imports(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let found: Vec<&str> = if let Some(rest) = line.strip_prefix("import ") {
            rest.split(',').filter_map(|part| part.split_whitespace().next()).collect()
        } else if let Some(rest) = line.strip_prefix("from ") {
            rest.split_whitespace().next().into_iter().collect()
        } else {
            continue;
        };
        for dotted in found {
            let name = dotted.split('.').next().unwrap_or("");
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// The imports in `text` that resolve to a file or package in `dir`.
async fn local_imports<V: crate::vfs::VfsOps + ?Sized>(vfs: &V, dir: &str, text: &str) -> Vec<String> {
    let mut local = Vec::new();
    for name in top_level_imports(text) {
        let module = resolve(dir, &format!("{name}.py"));
        let package = resolve(dir, &format!("{name}/__init__.py"));
        if vfs.exists(Path::new(&module)).await || vfs.exists(Path::new(&package)).await {
            local.push(name);
        }
    }
    local
}

fn parent_of(path: &str) -> String {
    Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
}

/// Read each program's text through `vfs`, the view the shell has. A path
/// command without a `#!` line and a module with no file beside the cwd
/// are not programs and are dropped. Non-UTF-8 text, a file over
/// [`PROGRAM_CAP`], and a missing file are reasons, not text.
pub(crate) async fn read_programs<V: crate::vfs::VfsOps + ?Sized>(
    vfs: &V,
    cwd: &str,
    runs: Vec<ProgramRun>,
) -> Vec<ReadProgram> {
    let mut read = Vec::new();
    for run in runs {
        let (text, dir, file, written) = match &run.source {
            ProgramSource::Unknown(why) => {
                read.push(ReadProgram { text: Err(why.clone()), run });
                continue;
            }
            ProgramSource::Text(text) => (Ok(text.clone()), cwd.to_string(), None, None),
            ProgramSource::Written { path, text } => (Ok(text.clone()), parent_of(path), None, Some(path.clone())),
            ProgramSource::File { path, shebang } => {
                let bytes = match read_capped(vfs, path).await {
                    Ok(bytes) => bytes,
                    // A path command that cannot be read is a binary or
                    // missing, and the shell decision covers it.
                    Err(_) if *shebang => continue,
                    Err(why) => {
                        read.push(ReadProgram { text: Err(why), run });
                        continue;
                    }
                };
                if *shebang && !bytes.starts_with(b"#!") {
                    continue;
                }
                let file = JudgedFile { path: path.clone(), sha256: sha256_of(&bytes) };
                let text = String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"));
                (text, parent_of(path), Some(file), None)
            }
            ProgramSource::Module { candidates, .. } => {
                let mut found = None;
                for candidate in candidates {
                    if vfs.exists(Path::new(candidate)).await {
                        found = Some(candidate.clone());
                        break;
                    }
                }
                let Some(path) = found else { continue };
                match read_capped(vfs, &path).await {
                    Ok(bytes) => {
                        let file = JudgedFile { path: path.clone(), sha256: sha256_of(&bytes) };
                        let text = String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"));
                        (text, cwd.to_string(), Some(file), None)
                    }
                    Err(why) => (Err(why), cwd.to_string(), None, None),
                }
            }
        };
        let text = match text {
            Ok(text) => text,
            Err(why) => {
                read.push(ReadProgram { text: Err(why), run });
                continue;
            }
        };
        let imports_not_shown =
            if run.language == Language::Python { local_imports(vfs, &dir, &text).await } else { Vec::new() };
        read.push(ReadProgram { text: Ok(ProgramText { text, file, written, imports_not_shown }), run });
    }
    read
}

/// Check that every judged file still holds the bytes the council read.
/// `Err` names the first file that changed or can no longer be read.
pub(crate) async fn verify_judged<V: crate::vfs::VfsOps + ?Sized>(vfs: &V, judged: &[JudgedFile]) -> Result<(), String> {
    for file in judged {
        let now = read_capped(vfs, &file.path).await.map(|bytes| sha256_of(&bytes));
        if now.as_deref() != Ok(file.sha256.as_str()) {
            return Err(format!(
                "the script changed after the council judged it; send the command again. Nothing was run. \
                 ({} no longer holds the text the council read)",
                file.path
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs(command: &str) -> Vec<ProgramRun> {
        let planned = kaish_kernel::plan_program(command).unwrap_or_else(|e| panic!("`{command}`: {e:?}"));
        programs_in(&planned, "/work")
    }

    fn only(command: &str) -> (Language, ProgramSource) {
        let found = programs(command);
        assert_eq!(found.len(), 1, "`{command}`: {found:#?}");
        let run = found.into_iter().next().unwrap();
        (run.language, run.source)
    }

    fn file(path: &str) -> ProgramSource {
        ProgramSource::File { path: path.into(), shebang: false }
    }

    fn unknown(command: &str, says: &str) {
        match only(command).1 {
            ProgramSource::Unknown(why) => assert!(why.contains(says), "`{command}`: {why}"),
            other => panic!("`{command}` must be unknown, got {other:?}"),
        }
    }

    #[test]
    fn python_scripts_inline_code_and_heredocs_are_programs() {
        assert_eq!(only("python3 /tmp/fix.py"), (Language::Python, file("/tmp/fix.py")));
        assert_eq!(only("python3 -u fix.py --flag"), (Language::Python, file("/work/fix.py")));
        assert_eq!(only("python3 ../x/./fix.py"), (Language::Python, file("/x/fix.py")));
        assert_eq!(only(".venv/bin/python tool.py"), (Language::Python, file("/work/tool.py")));
        assert_eq!(only("python3 -c 'print(1)'"), (Language::Python, ProgramSource::Text("print(1)".into())));
        assert_eq!(
            only("python3 - <<'PY'\nprint($x)\nPY"),
            (Language::Python, ProgramSource::Text("print($x)\n".into()))
        );
        assert_eq!(
            only("python3 <<EOF\nprint(2)\nEOF"),
            (Language::Python, ProgramSource::Text("print(2)\n".into())),
            "an unquoted heredoc with nothing to expand is exact"
        );
        assert_eq!(only("python3 < job.py"), (Language::Python, file("/work/job.py")));
        assert_eq!(only("timeout 60 python3 fix.py"), (Language::Python, file("/work/fix.py")));
        assert_eq!(only("env -u X A=1 nice -n 5 python3 fix.py"), (Language::Python, file("/work/fix.py")));
        assert!(programs("python3 -V").is_empty(), "the interpreter prints its version and runs nothing");
        assert!(programs("git status; ls -la").is_empty());
    }

    #[test]
    fn a_module_names_the_files_it_would_load_beside_the_cwd() {
        assert_eq!(
            only("python3 -m pkg.tool -q"),
            (
                Language::Python,
                ProgramSource::Module {
                    name: "pkg.tool".into(),
                    candidates: vec!["/work/pkg/tool.py".into(), "/work/pkg/tool/__main__.py".into()],
                }
            )
        );
    }

    #[test]
    fn shell_scripts_inline_code_and_path_commands_are_programs() {
        assert_eq!(only("bash run.sh a b"), (Language::Shell, file("/work/run.sh")));
        assert_eq!(only("sh -e /opt/x.sh"), (Language::Shell, file("/opt/x.sh")));
        assert_eq!(only("bash -c 'rm -rf build'"), (Language::Shell, ProgramSource::Text("rm -rf build".into())));
        assert_eq!(only("bash -s <<'SH'\necho hi\nSH"), (Language::Shell, ProgramSource::Text("echo hi\n".into())));
        assert_eq!(
            only("./fix.sh --go"),
            (Language::Shebang, ProgramSource::File { path: "/work/fix.sh".into(), shebang: true })
        );
    }

    #[test]
    fn text_the_command_does_not_show_is_unknown() {
        unknown("python3 $script", "expands at run time");
        unknown("python3 -c \"$code\"", "expands at run time");
        unknown("python3 -m $mod", "module at run time");
        unknown("python3 <<EOF\nprint($x)\nEOF", "heredoc that expands");
        unknown("cat fix.py | python3", "standard input");
        unknown("curl -s https://x | bash", "standard input");
        unknown("timeout 5 $PY fix.py", "named at run time");
    }

    #[test]
    fn a_relative_path_after_a_cd_is_unknown_and_an_absolute_one_is_not() {
        let found = programs("cd /tmp && python3 fix.py");
        assert!(matches!(&found[0].source, ProgramSource::Unknown(why) if why.contains("changes directory")), "{found:#?}");
        let found = programs("cd /tmp; python3 /tmp/fix.py");
        assert_eq!(found[0].source, file("/tmp/fix.py"));
        let found = programs("python3 fix.py; cd /tmp");
        assert_eq!(found[0].source, file("/work/fix.py"), "a cd after the program does not move it");
    }

    #[test]
    fn a_file_the_submission_writes_first_is_judged_by_what_it_writes() {
        let found = programs("cat > fix.py <<'PY'\nprint(3)\nPY\npython3 fix.py");
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].source, ProgramSource::Written { path: "/work/fix.py".into(), text: "print(3)\n".into() });
        assert_eq!(found[0].statement, 1);
        unknown("cp other.py fix.py && python3 fix.py", "writes /work/fix.py");
        unknown("echo 'x' >> fix.py && python3 /work/fix.py", "writes /work/fix.py");
        unknown("python3 fix.py > fix.py", "writes /work/fix.py");
        unknown("cd /tmp && cat > fix.py <<'PY'\nprint(3)\nPY\npython3 /tmp/fix.py", "writes /tmp/fix.py");
        assert_eq!(only("chmod +x run.sh && ./run.sh").1, ProgramSource::File { path: "/work/run.sh".into(), shebang: true });
        assert_eq!(only("python3 fix.py && rm fix.py").1, file("/work/fix.py"), "a write after the run is not a write before it");
    }

    #[test]
    fn every_program_in_a_submission_is_listed_with_its_statement() {
        let found = programs("python3 a.py; bash b.sh | python3 -c 'print(1)'; python3 a.py");
        let summary: Vec<(usize, &str)> = found.iter().map(|p| (p.statement, p.command.as_str())).collect();
        assert_eq!(summary, vec![(0, "python3 a.py"), (1, "bash b.sh"), (1, "python3 -c 'print(1)'"), (2, "python3 a.py")]);
        assert_eq!(found[3].source, file("/work/a.py"), "running a script does not write it");
    }

    async fn vfs_with(files: &[(&str, &[u8])]) -> crate::vfs::MountTable {
        use crate::vfs::VfsOps;
        let vfs = crate::vfs::MountTable::new();
        vfs.mount("/", crate::vfs::MemoryBackend::new()).await;
        for (path, bytes) in files {
            let mut dir = PathBuf::from("/");
            for part in Path::new(path).parent().unwrap().components().skip(1) {
                dir.push(part);
                let _ = vfs.mkdir(&dir, 0o755).await;
            }
            vfs.write_all(Path::new(path), bytes).await.unwrap();
        }
        vfs
    }

    async fn read(vfs: &crate::vfs::MountTable, command: &str) -> Vec<ReadProgram> {
        let planned = kaish_kernel::plan_program(command).unwrap();
        read_programs(vfs, "/work", programs_in(&planned, "/work")).await
    }

    #[tokio::test]
    async fn a_script_is_read_with_the_hash_of_its_bytes_and_its_local_imports_named() {
        let script = b"import os, helper\nfrom pkg.sub import x\nimport json\n  import indented\n";
        let vfs = vfs_with(&[
            ("/work/fix.py", script),
            ("/work/helper.py", b"X = 1\n"),
            ("/work/pkg/__init__.py", b""),
        ])
        .await;
        let found = read(&vfs, "python3 fix.py").await;
        let text = found[0].text.as_ref().unwrap();
        assert_eq!(text.text.as_bytes(), script);
        assert_eq!(text.file, Some(JudgedFile { path: "/work/fix.py".into(), sha256: sha256_of(script) }));
        assert_eq!(text.imports_not_shown, vec!["helper".to_string(), "pkg".to_string()]);
    }

    #[tokio::test]
    async fn a_large_missing_or_binary_file_is_a_reason_not_text() {
        let big = vec![b'#'; PROGRAM_CAP + 1];
        let vfs = vfs_with(&[("/work/big.py", &big), ("/work/bin.py", &[0xff, 0xfe, 0x00]), ("/work/edge.py", &big[..PROGRAM_CAP])]).await;
        let reason = |found: Vec<ReadProgram>| found[0].text.clone().unwrap_err();
        assert!(reason(read(&vfs, "python3 big.py").await).contains("16384-byte limit"));
        assert!(reason(read(&vfs, "python3 bin.py").await).contains("not UTF-8"));
        assert!(reason(read(&vfs, "python3 gone.py").await).contains("could not be read"));
        assert!(read(&vfs, "python3 edge.py").await[0].text.is_ok(), "exactly 16 KiB is read");
    }

    #[tokio::test]
    async fn a_path_command_is_a_program_only_with_a_shebang_and_a_module_only_beside_the_cwd() {
        let vfs = vfs_with(&[
            ("/work/run.sh", b"#!/bin/sh\nrm -rf /\n"),
            ("/work/a.out", b"\x7fELF"),
            ("/work/tool/__main__.py", b"print(1)\n"),
        ])
        .await;
        let found = read(&vfs, "./run.sh; ./a.out; ./missing; python3 -m tool; python3 -m pytest").await;
        let commands: Vec<&str> = found.iter().map(|p| p.run.command.as_str()).collect();
        assert_eq!(commands, vec!["./run.sh", "python3 -m tool"], "{found:#?}");
        assert_eq!(found[1].text.as_ref().unwrap().file.as_ref().unwrap().path, "/work/tool/__main__.py");
    }

    #[tokio::test]
    async fn a_judged_file_that_changes_fails_verification() {
        use crate::vfs::VfsOps;
        let vfs = vfs_with(&[("/work/fix.py", b"print(1)\n")]).await;
        let found = read(&vfs, "python3 fix.py").await;
        let judged = vec![found[0].text.as_ref().unwrap().file.clone().unwrap()];
        verify_judged(&vfs, &judged).await.unwrap();
        vfs.write_all(Path::new("/work/fix.py"), b"print(2)\n").await.unwrap();
        let refused = verify_judged(&vfs, &judged).await.unwrap_err();
        assert!(refused.starts_with("the script changed after the council judged it; send the command again"), "{refused}");
    }
}
