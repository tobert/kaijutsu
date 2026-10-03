//! `python3` and `python` as kaish wrapped commands, and the python program
//! a planned command runs.
//!
//! A shell that allows host exec registers each name that resolves on its
//! `PATH` when the shell is built. A name that does not resolve is not
//! registered, so it fails as a missing program does (exit 127). `python`
//! resolves on its own and never stands in for `python3`.
//!
//! The declaration follows the options `python3 -h` documents, with
//! `Tail::Forward`: every word reaches the interpreter in the order written,
//! so arguments after `-c CODE`, `-m MOD`, or the script pass to the program
//! unchanged. The child gets the environment and cwd an unwrapped external
//! gets: kaish's exported variables and the shell's real cwd, with no pins.
//!
//! Each call resolves the name again on the call's `PATH`, as an unwrapped
//! external does, so `export PATH="$VIRTUAL_ENV/bin:$PATH"` selects the
//! virtual environment's interpreter. See `docs/kaish-integration.md`,
//! "Wrapped python".

use std::path::PathBuf;

use anyhow::Result;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::wrapped::{find_executable, Flag, Positional, Stdin, Tail, Verb, WrappedCommand, WrappedTool};
use kaish_kernel::tools::{ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::Tool;
use kaish_types::plan::{PlannedCommand, PlannedValue};

/// The command names registered as wrapped python.
pub(crate) const PYTHON_NAMES: [&str; 2] = ["python3", "python"];

/// CPython switches. Each spelling is its own flag, so a word renders as
/// written; an alias would rewrite `-V` to `--version` inside a script's
/// arguments.
const SWITCHES: [(&str, &str); 23] = [
    ("-", "Read the program from standard input."),
    ("b", "Warn about str(bytes) comparisons."),
    ("B", "Do not write .pyc files."),
    ("d", "Turn on parser debugging output."),
    ("E", "Ignore PYTHON* environment variables."),
    ("h", "Print the interpreter's help and exit."),
    ("help", "Print the interpreter's help and exit."),
    ("help-env", "Print help about PYTHON* environment variables and exit."),
    ("help-xoptions", "Print help about -X options and exit."),
    ("help-all", "Print complete help and exit."),
    ("i", "Inspect interactively after running the program."),
    ("I", "Isolate from the environment and user site directory."),
    ("O", "Remove assert statements."),
    ("-OO", "Remove assert statements and docstrings."),
    ("P", "Do not prepend a potentially unsafe path to sys.path."),
    ("q", "Do not print the version and copyright on interactive startup."),
    ("s", "Do not add the user site directory to sys.path."),
    ("S", "Do not import the site module."),
    ("u", "Unbuffered standard output and standard error."),
    ("v", "Trace import statements."),
    ("V", "Print the interpreter version and exit."),
    ("version", "Print the interpreter version and exit."),
    ("x", "Skip the first line of the source."),
];

/// CPython options that take a value.
const VALUES: [(&str, &str); 4] = [
    ("c", "Run the program passed as a string."),
    ("m", "Run a library module as a script."),
    ("W", "Warning control."),
    ("X", "Set an implementation-specific option."),
];

/// The declaration for one name, without its executable.
fn declaration(name: &str) -> WrappedCommand {
    let mut root = Verb::root();
    for (flag, about) in SWITCHES {
        root = root.flag(Flag::switch(flag).repeatable().about(about));
    }
    for (flag, about) in VALUES {
        root = root.flag(Flag::value(flag).repeatable().about(about));
    }
    let root = root
        .positional(Positional::one("script").about("Program file to run."))
        .positional(Positional::many("args").about("Arguments for the program."))
        .tail(Tail::Forward)
        .stdin(Stdin::Pipe);
    WrappedCommand::new(name)
        .about("Run the Python interpreter found on PATH. Every argument reaches it as written.")
        .example("Run a script", format!("{name} script.py --verbose"))
        .example("Run inline code", format!("{name} -c 'print(1)'"))
        .root(root)
}

/// A wrapped python name. It holds the interpreter resolved when the shell
/// was built, and re-resolves the name on each call's `PATH`.
pub(crate) struct PythonTool {
    declaration: WrappedCommand,
    pinned: WrappedTool,
}

/// One tool per name in [`PYTHON_NAMES`] that resolves on `path`.
///
/// # Errors
///
/// A declaration `build()` refuses. That is a defect in this module, and the
/// shell must not be built without it.
pub(crate) fn python_tools(path: Option<&str>) -> Result<Vec<PythonTool>> {
    let Some(path) = path else { return Ok(Vec::new()) };
    PYTHON_NAMES.iter()
        .filter_map(|name| find_executable(name, path).map(|executable| (name, executable)))
        .map(|(name, executable)| {
            let declaration = declaration(name);
            let pinned = declaration.clone().executable(absolute(executable)).build()?;
            Ok(PythonTool { declaration, pinned })
        })
        .collect()
}

/// A `PATH` entry may be relative; kaish's external path resolves it against
/// the process cwd, and so does this.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

#[async_trait::async_trait]
impl Tool for PythonTool {
    fn name(&self) -> &str { self.pinned.name() }

    fn schema(&self) -> ToolSchema { self.pinned.schema() }

    fn validate(&self, args: &ToolArgs) -> Vec<kaish_kernel::validator::ValidationIssue> { self.pinned.validate(args) }

    async fn execute(&self, args: ToolArgs, ctx: &mut dyn ToolCtx) -> ExecResult {
        let name = self.pinned.name();
        let path = {
            let Some(ctx) = ctx.as_any_mut().downcast_mut::<kaish_kernel::tools::ExecContext>() else {
                return ExecResult::failure(1, format!("{name}: internal error: wrapped python requires ExecContext"));
            };
            // `--json` belongs to the program. kaish lifts it from a raw argv
            // into the output format; the interpreter's output stays as printed.
            ctx.output_format = None;
            ctx.scope.get("PATH").map(kaish_kernel::interpreter::value_to_string).unwrap_or_default()
        };
        let Some(executable) = find_executable(name, &path).map(absolute) else {
            return ExecResult::failure(127, format!("command not found: {name}"));
        };
        if executable == self.pinned.executable() {
            return self.pinned.execute(args, ctx).await;
        }
        match self.declaration.clone().executable(&executable).build() {
            Ok(tool) => tool.execute(args, ctx).await,
            Err(error) => ExecResult::failure(126, format!("{name}: {}: {error}", executable.display())),
        }
    }
}

/// Where a planned python command's program comes from, for `KJ_TOOL_PLAN`'s
/// `interpreter` field (`docs/kaish-integration.md`, "Wrapped python"). `None` when the
/// command does not name a python interpreter.
///
/// The command is named by its last path component, so `.venv/bin/python`
/// counts. Options are read as CPython reads them: clustered short options,
/// a value glued to its option or in the next word, and option parsing ends
/// at `-c`, `-m`, `-`, `--`, or the first operand. A word in option position
/// that kaish expands at run time makes the source `unknown`.
pub(crate) fn interpreter_source(command: &PlannedCommand) -> Option<serde_json::Value> {
    let base = command.name.rsplit('/').next().unwrap_or(&command.name);
    if !is_python_name(base) {
        return None;
    }
    let args = &command.args;
    let mut index = 0;
    let mut informational = false;
    let program = loop {
        let Some(arg) = args.get(index) else { break Program::Stdin };
        let Some(word) = arg.literal_value() else { break Program::Unknown };
        index += 1;
        if word == "-" {
            break Program::Stdin;
        }
        if word == "--" {
            break match args.get(index) {
                None => Program::Stdin,
                Some(script) if script.literal_value() == Some("-") => { index += 1; Program::Stdin }
                Some(script) if script.literal_value().is_some() => { index += 1; Program::Script(script.clone()) }
                Some(_) => Program::Unknown,
            };
        }
        if let Some(long) = word.strip_prefix("--") {
            let (option, glued) = match long.split_once('=') {
                Some((option, _)) => (option, true),
                None => (long, false),
            };
            informational |= matches!(option, "help" | "version" | "help-env" | "help-xoptions" | "help-all");
            if option == "check-hash-based-pycs" && !glued {
                // An option missing its value: CPython refuses the command.
                if index == args.len() {
                    break Program::Unknown;
                }
                index += 1;
            }
            continue;
        }
        let Some(cluster) = word.strip_prefix('-') else { break Program::Script(arg.clone()) };
        let mut takes = None;
        for (position, letter) in cluster.char_indices() {
            match letter {
                'c' | 'm' | 'W' | 'X' => { takes = Some((letter, &cluster[position + letter.len_utf8()..])); break; }
                'h' | '?' | 'V' => informational = true,
                _ => {}
            }
        }
        let Some((letter, glued)) = takes else { continue };
        let value = if glued.is_empty() {
            index += 1;
            args.get(index - 1).cloned()
        } else {
            Some(PlannedValue::literal(glued, glued))
        };
        match (letter, value) {
            ('c', Some(code)) => break Program::Inline(code),
            ('m', Some(module)) => break Program::Module(module),
            (_, None) => break Program::Unknown,
            _ => {}
        }
    };
    let rest = args.get(index..).unwrap_or_default().to_vec();
    let mut source = serde_json::json!({ "language": "python" });
    let fields = source.as_object_mut()?;
    let mut set = |key: &str, value: serde_json::Value| { fields.insert(key.to_string(), value); };
    let json = |value: &PlannedValue| serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    match program {
        _ if informational => set("source", "none".into()),
        Program::Inline(code) => {
            set("source", "inline".into());
            set("exact", code.literal_value().is_some().into());
            set("code", json(&code));
        }
        Program::Module(module) => {
            set("source", "module".into());
            set("module", json(&module));
        }
        Program::Script(script) => {
            set("source", "script".into());
            set("script", json(&script));
        }
        Program::Stdin => {
            set("source", "stdin".into());
            if let Some(heredoc) = command.heredocs.first() {
                set("exact", heredoc.literal.into());
                set("heredoc", heredoc.index.into());
                set("code", json(&heredoc.body));
            }
        }
        Program::Unknown => set("source", "unknown".into()),
    }
    let rest = if matches!(source["source"].as_str(), Some("unknown" | "none")) { args.clone() } else { rest };
    source["args"] = serde_json::Value::Array(rest.iter().map(json).collect());
    Some(source)
}

/// What a python command line runs.
enum Program {
    Inline(PlannedValue),
    Module(PlannedValue),
    Script(PlannedValue),
    Stdin,
    Unknown,
}

/// `python`, `python3`, `python3.14`, `python2`.
fn is_python_name(name: &str) -> bool {
    match name.strip_prefix("python") {
        Some("") => true,
        Some(version) => version.starts_with(|c: char| c.is_ascii_digit())
            && version.chars().all(|c| c.is_ascii_digit() || c == '.'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use kaish_kernel::ExecuteOptions;
    use kaish_kernel::interpreter::ExecResult;
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};

    use crate::block_store::shared_block_store;
    use crate::runtime::context_shell::ShellIdentity;
    use crate::runtime::embedded_kaish::{EmbeddedKaish, ExternalExec, OutputProfile};

    /// The host's `python3`, or `None` with a message: these tests run the
    /// real interpreter, and a host without one has nothing to check.
    fn host_python3() -> Option<PathBuf> {
        let found = std::env::var("PATH").ok()
            .and_then(|path| kaish_kernel::tools::wrapped::find_executable("python3", &path));
        if found.is_none() {
            eprintln!("SKIP: no python3 on the test process PATH; the python wrapper tests need one");
        }
        found
    }

    /// A shell with host exec granted, `PATH` set to `path`, and its cwd at
    /// `cwd` — the shape of a `shell_write` shell in a context holding `exec`.
    async fn exec_shell(path: &str, cwd: &Path) -> EmbeddedKaish {
        let principal = PrincipalId::system();
        let kernel = Arc::new(crate::Kernel::new_ephemeral("python-wrap").await);
        kernel.mount("/", crate::vfs::backends::LocalBackend::read_only("/")).await;
        EmbeddedKaish::with_identity(
            "python-wrap",
            shared_block_store(principal),
            kernel,
            Some(cwd.to_path_buf()),
            ShellIdentity { requester: principal, performer: principal, reviewer: None,
                context: ContextId::new(), session: SessionId::new() },
            crate::runtime::context_engine::session_context_map(),
            ExternalExec::Allow { path: Some(path.to_string()) },
            OutputProfile::Agent,
            |_, _, _| {},
        )
        .expect("build an exec shell")
    }

    async fn run(shell: &EmbeddedKaish, code: &str) -> ExecResult {
        shell.execute_with_options(code, ExecuteOptions::default()).await
            .unwrap_or_else(|error| panic!("`{code}` was refused before it ran: {error}"))
    }

    /// The host `PATH` the test process has, which the exec shells use.
    fn host_path() -> String {
        std::env::var("PATH").expect("the test process has a PATH")
    }

    /// A directory with a script and two stand-in modules. `pytest.py` and
    /// `pip.py` print their argv, so `python3 -m pytest` and `python3 -m pip`
    /// show what the interpreter received without a network or an install.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("script.py"),
            "import sys\nprint('script', sys.argv[1:])\nsys.exit(int(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1].isdigit() else 0)\n").unwrap();
        for module in ["pytest", "pip"] {
            std::fs::write(dir.path().join(format!("{module}.py")),
                format!("import sys\nprint('{module}', sys.argv[1:])\n")).unwrap();
        }
        dir
    }

    /// The source of each command's program, read as CPython reads its
    /// options. `(command, source, the program's word or null)`.
    #[test]
    fn interpreter_source_reads_options_as_cpython_does() {
        let cases: [(&str, Option<&str>, Option<&str>); 21] = [
            ("python3 -W", Some("unknown"), None),
            ("python3 -u -X", Some("unknown"), None),
            ("python3 --check-hash-based-pycs", Some("unknown"), None),
            ("python3 -- -", Some("stdin"), None),
            ("python3 -c 'print(1)' a", Some("inline"), Some("print(1)")),
            ("python3 -uc 'print(1)'", Some("inline"), Some("print(1)")),
            ("python3 '-cprint(1)'", Some("inline"), Some("print(1)")),
            ("python3 -B -W error -c x", Some("inline"), Some("x")),
            ("python3 -m pytest -q tests/", Some("module"), Some("pytest")),
            ("python3 -mcProfile x.py", Some("module"), Some("cProfile")),
            ("python3 -u script.py -c x", Some("script"), Some("script.py")),
            ("python3 -- '-weird.py'", Some("script"), Some("-weird.py")),
            ("python3 --check-hash-based-pycs never x.py", Some("script"), Some("x.py")),
            ("/app/.venv/bin/python x.py", Some("script"), Some("x.py")),
            ("python3.14 x.py", Some("script"), Some("x.py")),
            ("python3 < x.py", Some("stdin"), None),
            ("python3 -V", Some("none"), None),
            ("python3 --version", Some("none"), None),
            ("python3 $script", Some("unknown"), None),
            ("python3 -c", Some("unknown"), None),
            ("pythonic x.py", None, None),
        ];
        for (command, source, word) in cases {
            let plans = kaish_kernel::ast::plan::plan_program(command).expect(command);
            let found = super::interpreter_source(&plans[0].plan.commands[0]);
            assert_eq!(found.as_ref().and_then(|f| f["source"].as_str()), source, "`{command}`: {found:?}");
            let Some(found) = found else { continue };
            let program = ["code", "module", "script"].iter()
                .find_map(|key| found.get(*key)).and_then(|v| v["literal"]["value"].as_str());
            assert_eq!(program, word, "`{command}`: {found}");
        }
        let plans = kaish_kernel::ast::plan::plan_program("python3 -c 'print(1)' --json -v").unwrap();
        let found = super::interpreter_source(&plans[0].plan.commands[0]).unwrap();
        assert_eq!(found["args"], serde_json::json!([
            {"literal": {"text": "--json", "value": "--json"}}, {"literal": {"text": "-v", "value": "-v"}}]),
            "words after the code are the program's arguments");
        let plans = kaish_kernel::ast::plan::plan_program("python3 - <<EOF\nprint($x)\nEOF").unwrap();
        let found = super::interpreter_source(&plans[0].plan.commands[0]).unwrap();
        assert_eq!(found["exact"], false, "an unquoted heredoc expands before python reads it: {found}");
    }

    /// Every argv shape models use in recorded trajectories runs, prints
    /// what the interpreter printed, and exits with the interpreter's code.
    #[tokio::test]
    async fn the_shapes_models_write_run_with_their_output_and_exit_code() {
        if host_python3().is_none() { return; }
        let dir = project();
        let shell = exec_shell(&host_path(), dir.path()).await;
        let cwd = dir.path().display().to_string();

        let cases: Vec<(String, &str, i64)> = vec![
            ("python3 -c 'print(6*7)'".into(), "42\n", 0),
            ("python3 -c \"import sys\nprint('two')\nsys.exit(3)\"".into(), "two\n", 3),
            ("python3 - <<'EOF'\nimport sys\nprint('stdin', sys.argv)\nEOF".into(), "stdin ['-']\n", 0),
            ("python3 - a b <<'EOF'\nimport sys\nprint(sys.argv[1:])\nEOF".into(), "['a', 'b']\n", 0),
            ("python3 script.py 4 --flag -c x".into(), "script ['4', '--flag', '-c', 'x']\n", 4),
            ("python3 -m pytest -q tests/ -k 'a and b' -v -v -m slow -x".into(),
                "pytest ['-q', 'tests/', '-k', 'a and b', '-v', '-v', '-m', 'slow', '-x']\n", 0),
            ("python3 -m pip install x".into(), "pip ['install', 'x']\n", 0),
            ("python3 -u script.py".into(), "script []\n", 0),
            (format!("cd / && cd {cwd} && python -c 'import os; print(os.getcwd())'"), "", 0),
            ("python3 script.py a b c | tail -n 1".into(), "script ['a', 'b', 'c']\n", 0),
            ("python3 script.py '-cprint(1)' -vv -OO -Werror -- -m".into(),
                "script ['-cprint(1)', '-vv', '-OO', '-Werror', '--', '-m']\n", 0),
            ("python3 -OO -c 'import sys; print(sys.flags.optimize)'".into(), "2\n", 0),
            ("python3 -c 'import sys; print(sys.argv)' --json".into(), "['-c', '--json']\n", 0),
            ("python3 -c 'import sys; print(sys.argv)' -- -m x".into(), "['-c', '--', '-m', 'x']\n", 0),
        ];
        for (code, out, exit) in cases {
            let result = run(&shell, &code).await;
            assert_eq!(result.code, exit, "`{code}` exit code: {result:?}");
            if out.is_empty() {
                assert!(result.text_out().trim_end().ends_with(dir.path().file_name().unwrap().to_str().unwrap()),
                    "`{code}` must run in the cwd it changed to: {result:?}");
            } else {
                assert_eq!(result.text_out(), out, "`{code}` output: {result:?}");
            }
        }

        for code in ["python3 -V", "python3 --version"] {
            let result = run(&shell, code).await;
            assert!(result.ok(), "`{code}`: {result:?}");
            assert!(result.text_out().starts_with("Python 3."), "`{code}`: {result:?}");
        }
        let help = run(&shell, "python3 --help").await;
        assert!(help.ok() && help.text_out().starts_with("usage:"),
            "`python3 --help` prints the interpreter's usage, not kaish help: {help:?}");
        let failed = run(&shell, "python3 -c 'raise SystemExit(5)'").await;
        assert_eq!(failed.code, 5, "the interpreter's exit code passes through: {failed:?}");
    }

    /// `python3` and `python` name the wrapped command: `type` reports a
    /// kaish command rather than a file on `PATH`.
    #[tokio::test]
    async fn python3_and_python_resolve_to_the_wrapped_command() {
        if host_python3().is_none() { return; }
        let dir = tempfile::tempdir().unwrap();
        let shell = exec_shell(&host_path(), dir.path()).await;
        for name in ["python3", "python"] {
            if kaish_kernel::tools::wrapped::find_executable(name, &host_path()).is_none() { continue; }
            let result = run(&shell, &format!("type -t {name}")).await;
            assert_eq!(result.text_out().trim(), "builtin", "`{name}` must be the wrapped command: {result:?}");
        }
    }

    /// A name with no program on `PATH` is not registered, so it fails as
    /// a missing program does: exit 127, `command not found`. `python` is
    /// never pointed at `python3`.
    #[tokio::test]
    async fn a_name_missing_from_path_fails_as_a_missing_program() {
        let Some(python3) = host_python3() else { return };
        let bin = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&python3, bin.path().join("python3")).unwrap();
        let shell = exec_shell(&bin.path().display().to_string(), bin.path()).await;

        let missing = run(&shell, "python -c 'print(1)'").await;
        assert_eq!(missing.code, 127, "{missing:?}");
        assert!(missing.err.contains("command not found: python"), "{missing:?}");
        let typed = run(&shell, "type -t python").await;
        assert!(typed.text_out().trim().is_empty(), "`python` must not be registered: {typed:?}");

        let present = run(&shell, "python3 -c 'print(1)'").await;
        assert_eq!(present.text_out(), "1\n", "{present:?}");
        let typed = run(&shell, "type -t python3").await;
        assert_eq!(typed.text_out().trim(), "builtin", "{typed:?}");
    }

    /// The wrapped child sees the environment and cwd an unwrapped external
    /// sees: the same interpreter run by absolute path is the control.
    #[tokio::test]
    async fn the_child_environment_and_cwd_match_an_unwrapped_external() {
        let Some(python3) = host_python3() else { return };
        let dir = tempfile::tempdir().unwrap();
        let shell = exec_shell(&host_path(), dir.path()).await;
        let probe = "-c 'import json, os; print(json.dumps([os.getcwd(), dict(os.environ)], sort_keys=True))'";
        let setup = "export FOO=bar; export VIRTUAL_ENV=/nonexistent/venv; export PYTHONPATH=/nonexistent/lib; UNEXPORTED=1";

        let wrapped = run(&shell, &format!("{setup}; python3 {probe}")).await;
        let external = run(&shell, &format!("{setup}; {} {probe}", python3.display())).await;
        assert!(wrapped.ok() && external.ok(), "{wrapped:?} {external:?}");
        assert_eq!(wrapped.text_out(), external.text_out(), "wrapped and unwrapped children must agree");

        let seen: serde_json::Value = serde_json::from_str(&wrapped.text_out()).unwrap();
        assert_eq!(seen[0], dir.path().canonicalize().unwrap().display().to_string());
        let env = &seen[1];
        assert_eq!(env["FOO"], "bar");
        assert_eq!(env["VIRTUAL_ENV"], "/nonexistent/venv");
        assert_eq!(env["PYTHONPATH"], "/nonexistent/lib");
        assert_eq!(env["PATH"], host_path());
        assert!(env.get("UNEXPORTED").is_none(), "an unexported variable stays in the shell: {env}");
    }

    /// A virtual environment keeps working three ways: by path, by relative
    /// path, and by putting its `bin` first on `PATH` in the same command.
    /// A path is a different word from `python3`, so it runs unwrapped.
    #[tokio::test]
    async fn a_virtual_environment_runs_by_path_and_by_path_order() {
        let Some(python3) = host_python3() else { return };
        let dir = tempfile::tempdir().unwrap();
        // Fixture setup, outside the shell under test.
        let status = std::process::Command::new(&python3)
            .args(["-m", "venv", "--without-pip", ".venv"])
            .current_dir(dir.path())
            .status()
            .expect("create a venv fixture");
        assert!(status.success(), "python3 -m venv failed");
        let venv = dir.path().join(".venv");
        let shell = exec_shell(&host_path(), dir.path()).await;
        let prefix = "-c 'import sys; print(sys.prefix)'";
        let expected = format!("{}\n", venv.display());

        let absolute = run(&shell, &format!("{}/bin/python {prefix}", venv.display())).await;
        assert_eq!(absolute.text_out(), expected, "{absolute:?}");
        let relative = run(&shell, &format!(".venv/bin/python3 {prefix}")).await;
        assert_eq!(relative.text_out(), expected, "{relative:?}");

        let activated = run(&shell, &format!(
            "export VIRTUAL_ENV={}; export PATH=\"$VIRTUAL_ENV/bin:$PATH\"; python3 {prefix}", venv.display())).await;
        assert_eq!(activated.text_out(), expected, "PATH order picks the venv interpreter: {activated:?}");
        let prefixed = run(&shell, &format!("PATH={}/bin python {prefix}", venv.display())).await;
        assert_eq!(prefixed.text_out(), expected, "{prefixed:?}");
    }
}
