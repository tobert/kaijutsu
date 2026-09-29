//! `acp-fleet run [SCENARIO...]`: run ACP scenarios and report each one.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use kaijutsu_acp_fleet::run::{RunConfig, run_file};
use kaijutsu_acp_fleet::{DEFAULT_SCRATCH, FLEET_DIR, scenario};

/// Run ACP scenarios against an agent driven by a scripted model.
#[derive(Parser)]
#[command(name = "acp-fleet")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run scenarios and print PASS or FAIL for each. Exits 1 when any fails.
    Run {
        /// Scenario files, or directories whose *.toml files are scenarios.
        /// Default: the host scenarios shipped with this crate; contained ones
        /// are in its fleet/contained directory.
        scenarios: Vec<PathBuf>,
        /// The agent binary: kaijutsu-solo-acp built with --features test-mock.
        /// Default: kaijutsu-solo-acp next to this binary.
        #[arg(long)]
        agent: Option<PathBuf>,
        /// Where each run's scratch directory is made.
        #[arg(long, default_value = DEFAULT_SCRATCH)]
        scratch: PathBuf,
        /// Keep each run's scratch directory and print its path.
        #[arg(long)]
        keep: bool,
        /// Seconds to wait for each response, permission request, or expected text.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Print every ACP message to stderr as it is sent or received.
        #[arg(long)]
        trace: bool,
    },
}

fn main() -> ExitCode {
    match real_main() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("acp-fleet: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> Result<bool> {
    let Command::Run { scenarios, agent, scratch, keep, timeout, trace } = Cli::parse().command;
    let agent = match agent {
        Some(agent) => agent,
        None => sibling_agent()?,
    };
    let paths = if scenarios.is_empty() { vec![PathBuf::from(FLEET_DIR)] } else { scenarios };
    let files = scenario::discover(&paths)?;
    if files.is_empty() {
        bail!("no scenario files found in {paths:?}");
    }

    let mut config = RunConfig::new(agent, scratch);
    config.keep = keep;
    config.timeout = Duration::from_secs(timeout);
    config.trace = trace;

    let mut failed = 0;
    let mut gaps = 0;
    for file in &files {
        let outcome = run_file(file, &config);
        let seconds = outcome.elapsed.as_secs_f64();
        if outcome.passed() && let Some(finding) = &outcome.known_gap {
            gaps += 1;
            println!("GAP  {} ({finding}, {seconds:.1}s)", outcome.name);
            for excused in &outcome.excused {
                println!("  - {}", excused.replace('\n', "\n    "));
            }
        } else if outcome.passed() {
            println!("PASS {} ({seconds:.1}s)", outcome.name);
        } else {
            failed += 1;
            println!("FAIL {} ({seconds:.1}s)", outcome.name);
            for failure in &outcome.failures {
                println!("  - {}", failure.replace('\n', "\n    "));
            }
            if !outcome.stderr_tail.is_empty() {
                println!("  agent stderr (tail):\n    {}", outcome.stderr_tail.replace('\n', "\n    "));
            }
        }
        if let Some(kept) = &outcome.kept {
            println!("  kept {}", kept.display());
        }
    }
    println!("{} passed ({gaps} as known gaps), {failed} failed", files.len() - failed);
    Ok(failed == 0)
}

/// `kaijutsu-solo-acp` in the directory this binary runs from.
fn sibling_agent() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("find this binary's path")?;
    let dir = exe.parent().context("this binary's path has no parent directory")?;
    let agent = dir.join("kaijutsu-solo-acp");
    if !agent.is_file() {
        bail!(
            "no agent at {}; build one with `cargo build -p kaijutsu-solo-acp --features test-mock` \
             or pass --agent",
            agent.display()
        );
    }
    Ok(agent)
}
