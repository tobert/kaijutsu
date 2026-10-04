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
}
