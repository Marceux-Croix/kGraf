//! `kgraf land`: land a run onto the user's branch (decision 0100, section
//! 5), with no live loop, under the run's lease. The one land function
//! (`kgraf_orchestrator::land_flow::land`, through
//! `kgraf_host::land::land_project_run`) does the work; this file parses the
//! command, prints the outcome, and asks the user's yes for an untested land.
//!
//! Also the land suggestion at the end of an `--interactive` run: a `y`/`n`
//! prompt on the run's own stdin channel. A run without `--interactive`
//! prints nothing new.

use anyhow::{Result, bail};
use kgraf_core::RunId;
use kgraf_host::land::{land_project_run, project_suggest_facts, record_land_decline};
use kgraf_orchestrator::land_flow::{LandOutcome, LandRequest, UntestedBinding};
use kgraf_orchestrator::land_suggest::{SuggestAnswer, Suggester};
use std::io::IsTerminal;
use std::path::Path;

#[derive(clap::Args, Debug)]
pub struct LandArgs {
    /// The run to land.
    #[arg(long)]
    pub run: String,
    /// The branch to land into. Default: the branch checked out now.
    #[arg(long)]
    pub into: Option<String>,
    /// Land this commit of the run's branch instead of the newest one the
    /// root gate passed on.
    #[arg(long)]
    pub commit: Option<String>,
    /// Land even though no gate passed on what lands (after the warning).
    /// The merge commit is marked UNTESTED. Never with nobody at the
    /// terminal.
    #[arg(long)]
    pub untested: bool,
    /// Recover the worktree a land stopped part way left (files partly
    /// written, a stale index.lock), then land again.
    #[arg(long, conflicts_with_all = ["untested", "commit", "into"])]
    pub recover: bool,
}

/// `kgraf land`.
pub fn run(root: &Path, args: LandArgs) -> Result<()> {
    if args.recover {
        return recover(root, &RunId::new(&args.run));
    }
    let request = LandRequest {
        into: args.into,
        candidate: args.commit,
        attended: std::io::stdin().is_terminal(),
        ..LandRequest::default()
    };
    // `--untested` answers the question ahead of time, but the warning is
    // still printed first and the yes is bound to what it names.
    let mut ask = |q: &str| {
        if args.untested && q.ends_with("untested?") {
            eprintln!("{q} yes (--untested)");
            Some("y".to_owned())
        } else {
            ask_terminal(q)
        }
    };
    land_run(root, &RunId::new(&args.run), request, &mut ask)
}

fn recover(root: &Path, run: &RunId) -> Result<()> {
    let r = kgraf_host::land::recover_project_run(root, run)?;
    for p in &r.restored {
        println!("restored   {p}");
    }
    for p in &r.removed {
        println!("removed    {p}");
    }
    for (p, why) in &r.left {
        println!("left alone {p}: {why}");
    }
    if let Some(lock) = &r.lock {
        println!("lock       {lock}");
    }
    println!(
        "next       check the files left alone, then `kgraf land --run {run}` finishes the land"
    );
    Ok(())
}

fn ask_terminal(question: &str) -> Option<String> {
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    Some(line)
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes")
}

/// Land `run` and print the outcome. `ask` reads the user's answer to the
/// untested warning (`None`: nobody answered).
pub fn land_run(
    root: &Path,
    run: &RunId,
    mut request: LandRequest,
    ask: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<()> {
    loop {
        match land_project_run(root, run, &request)? {
            LandOutcome::Landed { entry, notices } => {
                for n in notices {
                    println!("notice     {n}");
                }
                println!(
                    "landed     {} of {} into {} as merge commit {}",
                    short(&entry.candidate),
                    entry.run_branch,
                    entry.target,
                    entry.merge_commit
                );
                if let Some(fold) = entry.fold {
                    println!("run branch {fold}");
                }
                return Ok(());
            }
            LandOutcome::NothingToLand { candidate, target } => {
                println!(
                    "nothing to land: {} is already on {target}",
                    short(&candidate)
                );
                return Ok(());
            }
            LandOutcome::Conflict { target, paths } => bail!(
                "run {run} does not merge cleanly into {target} (conflicts: {}); nothing moved. \
                 Resume the run so a resolver merges {target} into its branch, then land again",
                paths.join(", ")
            ),
            LandOutcome::Remediating { contract, target } => {
                println!(
                    "waiting    the gate failed on the merge with {target}; remediation \
                     {contract} is open"
                );
                return Ok(());
            }
            LandOutcome::NeedsUntestedYes {
                gate,
                reason,
                candidate,
                target,
                target_tip,
                commits,
            } => {
                let what = what_lands(&candidate, commits, &target);
                eprintln!("WARNING    root gate {gate} did not pass on what would land: {reason}");
                eprintln!(
                    "WARNING    landing {what} now lands UNTESTED code, marked so in the merge \
                     commit"
                );
                let yes = request.attended
                    && ask(&format!("Land {what} untested?")).is_some_and(|a| is_yes(&a));
                if !yes {
                    bail!(
                        "not landed. `kgraf land --run {run} --untested` lands it untested; \
                         resuming the run (`kgraf orchestrate --resume {run} ...`) lets the \
                         gate's remediation run first"
                    );
                }
                // The yes is for this candidate, target and target tip only.
                request.candidate = Some(candidate.clone());
                request.into = Some(target.clone());
                request.untested_confirmed = true;
                request.untested_for = Some(UntestedBinding {
                    candidate,
                    target,
                    target_tip,
                });
            }
            LandOutcome::NeedsProgramsYes {
                changes,
                candidate,
                target,
                commits,
            } => {
                let what = what_lands(&candidate, commits, &target);
                eprintln!(
                    "WARNING    landing {what} runs your git's hooks, filters or signing \
                     programs, and these changed since the run started (a worker could have \
                     changed them):"
                );
                for c in &changes {
                    eprintln!("WARNING      - {c}");
                }
                let yes = request.attended
                    && ask("Run them as changed, to land?").is_some_and(|a| is_yes(&a));
                if !yes {
                    bail!("not landed: check the changes listed above first");
                }
                request.candidate = Some(candidate);
                request.into = Some(target);
                request.programs_confirmed = changes;
            }
        }
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// `<short sha> (<n> commits) into <branch>`, as every question names it.
fn what_lands(candidate: &str, commits: usize, target: &str) -> String {
    format!(
        "{} ({commits} commit{}) into {target}",
        short(candidate),
        if commits == 1 { "" } else { "s" }
    )
}

/// After an `--interactive` run: offer to land when a rule fires, and land
/// on `y` only. `answer` reads one line from the run's stdin channel. A
/// benchmark run raises no suggestion.
pub fn offer_after_run(
    root: &Path,
    run: &RunId,
    benchmark: bool,
    answer: &mut dyn FnMut() -> Option<String>,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (record, facts) = match project_suggest_facts(root, run, benchmark, now) {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(e) => {
            println!("notice     no land suggestion: {e}");
            return;
        }
    };
    let mut suggester = Suggester::default();
    let Some(suggestion) = suggester.evaluate(&record.land, &facts) else {
        return;
    };
    println!(
        "land?      {}. Land {} of run {run} as one merge commit? (y/n)",
        suggestion.rule.reason(),
        what_lands(
            facts.candidate.as_deref().unwrap_or_default(),
            facts.commits,
            facts.target.as_deref().unwrap_or_default()
        )
    );
    // No answer (stdin closed) is neither a yes nor a decline.
    let Some(reply) = answer() else {
        println!("no answer  nothing landed; `kgraf land --run {run}` lands it later");
        return;
    };
    let choice = if is_yes(&reply) {
        SuggestAnswer::Land
    } else {
        SuggestAnswer::NotNow
    };
    let mut book = record.land.clone();
    // The mode never lands for the user; only the explicit `y` does.
    let land = suggester.answer(
        &mut book,
        kgraf_tools::PermissionMode::default(),
        choice,
        now,
        facts.verified,
    );
    if !land {
        if let Err(e) = record_land_decline(root, run, now) {
            println!("notice     the decline was not recorded: {e}");
        }
        println!("not now    `kgraf land --run {run}` lands it later");
        return;
    }
    let request = LandRequest {
        attended: true,
        ..LandRequest::default()
    };
    let mut no_second_question = |_: &str| None;
    if let Err(e) = land_run(root, run, request, &mut no_second_question) {
        println!("not landed {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_question_names_the_candidate_count_and_target() {
        assert_eq!(
            what_lands("0123456789abcdef", 2, "main"),
            "0123456789ab (2 commits) into main"
        );
        assert_eq!(what_lands("abc", 1, "dev"), "abc (1 commit) into dev");
    }

    #[test]
    fn only_an_explicit_yes_is_a_yes() {
        assert!(is_yes("y\n") && is_yes("yes") && is_yes(" Y "));
        assert!(!is_yes("") && !is_yes("n") && !is_yes("yeah") && !is_yes("land"));
    }
}
