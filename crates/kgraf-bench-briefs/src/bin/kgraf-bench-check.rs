//! `kgraf-bench-check`: judge an attempt made outside kGraf with the same
//! mechanical check `kgraf-bench-briefs` applies to its own attempts.
//!
//! A baseline harness (aider, for example) edits a copy of a fixture. The
//! caller commits that copy in a repository whose first commit is the
//! fixture, then passes the repository, the fixture commit and the attempt
//! commit here. The same `check::verify` runs: protected paths compared
//! against the fixture commit, build inputs refused, tests run in the
//! sandbox, mutants applied for a `Tests` brief.
//!
//! `--list` prints the suite (ids, kinds, versions, instructions, protected
//! paths and the suite hash) as JSON so a baseline driver reads the brief text
//! from the one place it is defined.
//!
//! Exit status: 0 satisfied, 1 not satisfied, 2 the check itself could not
//! run (a harness fault, never a measurement).

use anyhow::{Context, Result, bail};
use clap::Parser;
use kgraf_bench_briefs::{brief, check, gitutil};
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Args {
    /// Print the brief suite as JSON and exit.
    #[arg(long)]
    list: bool,
    /// Run the check's positive control (a known-good crate) and exit.
    #[arg(long)]
    self_test: bool,
    /// The brief id to judge against.
    #[arg(long)]
    brief: Option<String>,
    /// The repository holding the fixture commit and the attempt commit.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// The fixture commit (the state the attempt started from).
    #[arg(long)]
    base: Option<String>,
    /// The attempt commit.
    #[arg(long, default_value = "HEAD")]
    commit: String,
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({ "satisfied": null, "error": format!("{e:#}") })
            );
            std::process::exit(2);
        }
    }
}

fn run() -> Result<i32> {
    let args = Args::parse();
    if args.list {
        let suite: Vec<_> = brief::suite()
            .iter()
            .map(|b| {
                serde_json::json!({
                    "id": b.id,
                    "kind": b.kind.as_str(),
                    "version": b.version,
                    "instruction": b.instruction,
                    "protected": b.protected,
                    "fixture_dir": b.fixture_dir(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "suite_hash": brief::suite_hash(),
                "briefs": suite,
            }))?
        );
        return Ok(0);
    }
    if args.self_test {
        check::self_test(check::DEFAULT_TIMEOUT).context("the check harness positive control")?;
        println!("{}", serde_json::json!({ "self_test": "ok" }));
        return Ok(0);
    }
    let id = args.brief.context("--brief is required")?;
    let repo = args.repo.context("--repo is required")?;
    let base = args.base.context("--base is required")?;
    let Some(brief) = brief::suite().into_iter().find(|b| b.id == id) else {
        bail!("no brief `{id}` in the suite");
    };
    let scratch_root = tempfile::tempdir().context("cannot create a scratch directory")?;
    let checked_out = scratch_root.path().join("checked-out");
    gitutil::checkout_commit(&repo, &args.commit, &checked_out)
        .with_context(|| format!("cannot check out {}", args.commit))?;
    let scratch = scratch_root.path().join("check-scratch");
    std::fs::create_dir_all(&scratch).context("cannot create the check scratch directory")?;
    let verdict = check::verify(
        &brief,
        &check::Attempt {
            dir: &checked_out,
            repo: &repo,
            base: &base,
            commit: &args.commit,
        },
        &check::CheckEnv {
            scratch: &scratch,
            timeout: check::DEFAULT_TIMEOUT,
        },
    )?;
    match verdict {
        check::Verdict::Satisfied => {
            println!(
                "{}",
                serde_json::json!({ "satisfied": true, "reason": null })
            );
            Ok(0)
        }
        check::Verdict::Unsatisfied(reason) => {
            println!(
                "{}",
                serde_json::json!({ "satisfied": false, "reason": reason })
            );
            Ok(1)
        }
    }
}
