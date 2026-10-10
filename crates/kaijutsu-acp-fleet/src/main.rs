//! `acp-fleet run [SCENARIO...]`: run ACP scenarios and report each one.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use kaijutsu_acp_fleet::run::{RunConfig, run_file};
use kaijutsu_acp_fleet::{CONTAINED_LIVE_DIR, DEFAULT_SCRATCH, FLEET_DIR, scenario};

/// Run ACP scenarios against an agent driven by a scripted model, or by a
/// real model API for a `[live]` scenario.
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
        /// are in its fleet/contained directory, and live ones in
        /// fleet/contained/live. A live scenario named here runs without
        /// --live.
        scenarios: Vec<PathBuf>,
        /// Also run the live scenarios in fleet/contained/live, which talk to
        /// a real model API from a container and spend money. With no
        /// scenarios named, they are skipped and the run says so.
        #[arg(long)]
        live: bool,
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
    let Command::Run { scenarios, live, agent, scratch, keep, timeout, trace } = Cli::parse().command;
    let agent = match agent {
        Some(agent) => agent,
        None => sibling_agent()?,
    };
    let named = !scenarios.is_empty();
    let live_dirs = [PathBuf::from(CONTAINED_LIVE_DIR)];
    let paths = match (named, live) {
        (true, true) => [scenarios, live_dirs.to_vec()].concat(),
        (true, false) => scenarios,
        (false, true) => [vec![PathBuf::from(FLEET_DIR)], live_dirs.to_vec()].concat(),
        (false, false) => vec![PathBuf::from(FLEET_DIR)],
    };
    let files = scenario::discover(&paths)?;
    if files.is_empty() {
        bail!("no scenario files found in {paths:?}");
    }
    if !named && !live {
        let skipped = scenario::discover(&live_dirs)?;
        if !skipped.is_empty() {
            println!(
                "SKIP {} live scenario(s) in {CONTAINED_LIVE_DIR}; they spend money, so pass --live or name one",
                skipped.len()
            );
        }
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
        if outcome.passed() && !outcome.known_gaps.is_empty() {
            gaps += 1;
            println!("GAP  {} ({}, {seconds:.1}s)", outcome.name, outcome.known_gaps.join(", "));
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
        for note in &outcome.notes {
            println!("  {note}");
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
