//! One run segment from assembly to its recorded end, with the end guaranteed.
//!
//! The end-of-run commit used to run only when everything
//! before it succeeded, so an error in assembly, in the transcript or in the
//! loop left a worktree no commit carried, which the next resume's `create`
//! then purged while the transcript still described its edits. Here every exit
//! path after the worktree exists ends the same way: [`run_segment`] assembles,
//! runs and then, whatever happened, commits or keeps the worktree
//! (`workspace::finish`), records the end in the transcript, checks MCP
//! attribution, and closes the MCP connections. Errors are collected, not
//! returned early. A process killed outright (SIGKILL, power loss) still cannot
//! run this; its segment stays unended in the transcript, and resume refuses
//! it rather than purge it.

use super::assemble::{Assembled, Assembly, assemble};
use super::run::{CancelWhen, OpenTranscript, SegmentEnd, run_worker};
use super::workspace::{self, Segment};
use kgraf_runtime::{WorkerBrief, WorkerEvent, WorkerResult};
use kgraf_sandbox::{MergeBack, WorkspaceManager};

/// Where [`conclude`] stops, as a process killed at that point would. Tests
/// use it to check that each window leaves a state a resume recovers from;
/// production passes [`Fault::None`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    /// After merge-back committed, before the end is recorded.
    AfterCommit,
    /// After the end is recorded, before the worktree is removed.
    AfterRecord,
}

/// End a segment in the order that leaves every intermediate state
/// recoverable: commit through merge-back (the attempt ref is written), then
/// record the end in the transcript, then remove the worktree.
///
/// - Killed after the commit: the transcript is unended, the attempt ref
///   exists and starts from the recorded start; `plan_segment` recovers the
///   committed end and records it.
/// - Killed after the record: the end is recorded; the worktree is a leftover
///   under its own name, which the next segment (the next number) never
///   reuses.
///
/// A record that fails keeps the worktree; the error is returned with the end.
pub async fn conclude(
    manager: &WorkspaceManager,
    merge_back: &MergeBack,
    mut segment: Segment,
    opened: &mut OpenTranscript,
    message: &str,
    fault: Fault,
) -> (SegmentEnd, anyhow::Result<()>) {
    let name = segment.workspace().name().to_owned();
    let end = workspace::commit(merge_back, &mut segment, message);
    if fault == Fault::AfterCommit {
        return (end, Ok(()));
    }
    if let Err(e) = opened.end_segment(&name, end.clone()).await {
        return (
            end,
            Err(e.context("the segment's end could not be recorded; its worktree is kept")),
        );
    }
    if fault == Fault::AfterRecord {
        return (end, Ok(()));
    }
    let removed = match &end {
        SegmentEnd::Committed { .. } => workspace::remove(manager, segment),
        // Empty: merge-back removed it. Refused or not carried: kept.
        _ => Ok(()),
    };
    (end, removed)
}

/// How to run the segment.
pub struct SegmentOptions {
    pub resume: bool,
    pub cancel: Option<CancelWhen>,
    pub cancel_on_interrupt: bool,
    /// The commit message prefix for the segment's attempt.
    pub message: String,
}

/// What a segment did. `errors` holds every failure, in the order met; a
/// segment with errors still has a recorded `end`.
pub struct SegmentReport {
    pub result: Option<WorkerResult>,
    pub end: SegmentEnd,
    pub errors: Vec<anyhow::Error>,
}

/// Assemble, run and always end one segment (see the module docs).
/// `on_assembled` sees the environment before the worker starts.
#[allow(clippy::too_many_arguments)]
pub async fn run_segment(
    assembly: Assembly,
    segment: Segment,
    manager: &WorkspaceManager,
    merge_back: &MergeBack,
    opened: &mut OpenTranscript,
    brief: WorkerBrief,
    options: SegmentOptions,
    on_assembled: impl FnOnce(&Assembled),
    on_event: impl FnMut(&WorkerEvent),
) -> SegmentReport {
    let mut errors = Vec::new();
    let mut result = None;
    let assembled = match assemble(assembly).await {
        Ok(assembled) => Some(assembled),
        Err(e) => {
            errors.push(e.context("assembling the worker failed"));
            None
        }
    };
    if let Some(assembled) = &assembled {
        on_assembled(assembled);
        match run_worker(
            assembled.env.clone(),
            brief,
            opened,
            options.resume,
            options.cancel,
            options.cancel_on_interrupt,
            on_event,
        )
        .await
        {
            Ok(r) => result = Some(r),
            Err(e) => errors.push(e.context("the worker run failed")),
        }
    }

    let outcome = match &result {
        Some(r) => serde_json::to_string(&r.outcome).unwrap_or_else(|_| "?".into()),
        None => "no result".into(),
    };
    let (end, concluded) = conclude(
        manager,
        merge_back,
        segment,
        opened,
        &format!("{}: {outcome}", options.message),
        Fault::None,
    )
    .await;
    if let Err(e) = concluded {
        errors.push(e);
    }
    if let Some(assembled) = assembled {
        if let Err(e) = assembled.attribution.check() {
            errors.push(e.into());
        }
        assembled.close().await;
    }
    SegmentReport {
        result,
        end,
        errors,
    }
}
