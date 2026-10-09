//! Work an earlier merge of the same run already delivered (decision 0086).
//!
//! Two contracts of one plan can write the same file: a handoff child
//! implements the whole file, and its sibling is dispatched afterwards on a
//! tree where the sibling's check already passes. The verifier refuses an
//! attempt that changed none of its declared files, because a check that
//! passes before the work proves nothing. That refusal is wrong when an
//! earlier merge of the run is what made the check pass, and the evidence is
//! in the integration branch: every merge is a `Merge attempt ... into <branch>`
//! commit on its first-parent line, so the commit the run started from is the
//! first commit of that line that is not one.
//!
//! The same history names the earlier merges a failing check may come from:
//! when a check fails on an attempt and also at its dispatch base, the
//! verifier reads every merge of the run before that base
//! ([`merged_before`]) to say which deliveries the base holds.

use kgraf_sandbox::git::Git;
use std::path::Path;

/// The merges of a run before a contract was dispatched: those that changed
/// its declared files ([`find`]), or all of them ([`merged_before`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EarlierDelivery {
    /// The commit the run's integration branch started from.
    pub(crate) original_base: String,
    /// The contracts whose merges are meant, oldest first.
    pub(crate) contracts: Vec<String>,
}

/// The subject of the commit merge-back makes for an attempt
/// (`kgraf: contract <id>`).
const ATTEMPT_SUBJECT: &str = "kgraf: contract ";

/// The run's integration history up to a dispatch base.
struct RunHistory {
    /// The commit the run started from.
    original_base: String,
    /// The run's merges after it as (merge, its first parent, its second
    /// parent), newest first.
    merges: Vec<(String, String, String)>,
}

/// The run's history on the first-parent line of `dispatch_base`, or `None`
/// when every commit of that line is a merge of the run.
fn history(
    git: &Git,
    repo: &Path,
    branch: &str,
    dispatch_base: &str,
) -> Result<Option<RunHistory>, String> {
    let log = git
        .run(
            repo,
            [
                "log",
                "--first-parent",
                "-z",
                "--format=%H%x1f%P%x1f%s",
                dispatch_base,
            ],
        )
        .map_err(|e| format!("cannot read the integration history at {dispatch_base}: {e}"))?;
    let suffix = format!(" into {branch}");
    let mut merges: Vec<(String, String, String)> = Vec::new();
    for record in log.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8_lossy(record);
        let mut fields = record.splitn(3, '\u{1f}');
        let (Some(sha), Some(parents), Some(subject)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let parents: Vec<&str> = parents.split_whitespace().collect();
        let is_merge = parents.len() == 2
            && subject.starts_with("Merge attempt ")
            && subject.ends_with(&suffix);
        if is_merge {
            merges.push((sha.to_owned(), parents[0].to_owned(), parents[1].to_owned()));
        } else {
            return Ok(Some(RunHistory {
                original_base: sha.to_owned(),
                merges,
            }));
        }
    }
    Ok(None)
}

/// The contract an attempt commit (a merge's second parent) was made for.
fn contract_of(git: &Git, repo: &Path, attempt: &str) -> Result<String, String> {
    let subject = git
        .run(repo, ["log", "-1", "--format=%s", attempt])
        .map_err(|e| format!("cannot read the attempt commit {attempt}: {e}"))?;
    let subject = String::from_utf8_lossy(&subject).trim().to_owned();
    Ok(subject
        .strip_prefix(ATTEMPT_SUBJECT)
        .unwrap_or(&subject)
        .to_owned())
}

/// What merged the declared `paths` into the integration `branch` between
/// the run's start and `dispatch_base`, or `None` when nothing did: no merge
/// of the run precedes `dispatch_base`, or none touched the paths. A git
/// failure is an error, never read as "nothing delivered".
pub(crate) fn find(
    repo: &Path,
    branch: &str,
    dispatch_base: &str,
    paths: &[&str],
) -> Result<Option<EarlierDelivery>, String> {
    let git = Git::discover().map_err(|e| e.to_string())?;
    let Some(RunHistory {
        original_base,
        merges,
    }) = history(&git, repo, branch, dispatch_base)?
    else {
        return Ok(None);
    };
    if merges.is_empty() || paths.is_empty() {
        return Ok(None);
    }
    let specs: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    let changed = |from: &str, to: &str| -> Result<bool, String> {
        let mut args: Vec<&str> = vec!["diff", "--name-only", from, to, "--"];
        args.extend(specs.iter().map(String::as_str));
        let out = git
            .run(repo, args)
            .map_err(|e| format!("cannot compare {from} with {to}: {e}"))?;
        Ok(!out.iter().all(u8::is_ascii_whitespace))
    };
    if !changed(&original_base, dispatch_base)? {
        return Ok(None);
    }
    let mut contracts = Vec::new();
    for (merge, first, second) in merges.iter().rev() {
        if !changed(first, merge)? {
            continue;
        }
        contracts.push(contract_of(&git, repo, second)?);
    }
    if contracts.is_empty() {
        return Ok(None);
    }
    Ok(Some(EarlierDelivery {
        original_base,
        contracts,
    }))
}

/// Every merge of the run into the integration `branch` before
/// `dispatch_base`, whatever files it changed: the commit the run started
/// from, and the merged contracts oldest first (none when `dispatch_base` is
/// that commit). A git failure, or a first-parent line with no commit the
/// run started from, is an error.
pub(crate) fn merged_before(
    repo: &Path,
    branch: &str,
    dispatch_base: &str,
) -> Result<EarlierDelivery, String> {
    let git = Git::discover().map_err(|e| e.to_string())?;
    let Some(RunHistory {
        original_base,
        merges,
    }) = history(&git, repo, branch, dispatch_base)?
    else {
        return Err(format!(
            "no commit the run started from is on the first-parent line of {dispatch_base}"
        ));
    };
    let contracts = merges
        .iter()
        .rev()
        .map(|(_, _, second)| contract_of(&git, repo, second))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EarlierDelivery {
        original_base,
        contracts,
    })
}
