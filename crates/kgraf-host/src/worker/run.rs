//! A worker's durable transcript, and running one worker against it.
//!
//! The transcript is `kgraf-session`'s `JsonlTranscript` (ported from
//! codex-rs `rollout/`): one record per line, each written and flushed before
//! the next is taken, under an exclusive lock for the file's lifetime. **Resume
//! reads it**: the worker events it holds are what the loop rebuilds its
//! conversation from, and the new segment's records are appended to the same
//! file.
//!
//! Beside the loop's own events it records what the loop cannot see:
//!
//! - [`Record::Owner`], first: the worker, the task and a digest of the brief.
//!   A resume by anyone else, or of a different brief, is refused.
//! - [`Record::SegmentStart`] and [`Record::SegmentEnd`]: which worktree each run
//!   segment used, what it started from, and how it ended (the attempt commit,
//!   nothing changed, refused, or not carried). A segment without a recorded
//!   end is recovered when the repository proves how it ended (its own
//!   attempt ref, starting from its recorded start; or no ref and no worktree,
//!   merge-back's empty outcome) and refused otherwise, since its worktree
//!   holds work no commit carries. An attempt ref that no longer matches the
//!   recorded end is refused (item 1).
//!
//! A torn last line (a process killed mid-write) is repaired on open, and what
//! was discarded is returned so the caller shows it; corruption
//! anywhere else refuses the resume.

use anyhow::{Context, Result, bail};
use kgraf_runtime::{
    SpawnOptions, TranscriptSettler, WorkerBrief, WorkerEnv, WorkerEvent, WorkerResult,
    spawn_worker,
};
use kgraf_session::transcript::{JsonlTranscript, TornTail, Transcript};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Who a transcript belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub worker: String,
    pub task: String,
    /// blake3 of the brief's instruction, hex.
    pub brief_digest: String,
}

impl Owner {
    pub fn new(worker: &str, task: &str, instruction: &str) -> Self {
        Self {
            worker: worker.to_owned(),
            task: task.to_owned(),
            brief_digest: blake3::hash(instruction.as_bytes()).to_hex().to_string(),
        }
    }
}

/// How a run segment ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SegmentEnd {
    /// Committed by merge-back under `attempt_ref`; the worktree is removed.
    Committed { attempt_ref: String, commit: String },
    /// Nothing changed; no attempt ref, the worktree is removed.
    Empty,
    /// Merge-back refused the content; the worktree is kept at `kept`.
    Refused { reasons: String, kept: String },
    /// The commit itself failed; the worktree is kept at `kept`.
    NotCarried { error: String, kept: String },
}

/// One transcript line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    Owner(Owner),
    SegmentStart { segment: String, start: String },
    Event { event: WorkerEvent },
    SegmentEnd { segment: String, end: SegmentEnd },
}

/// A worker's transcript file, opened for appending, with what it already holds.
pub struct OpenTranscript {
    pub transcript: JsonlTranscript<Record>,
    /// Every record already stored, oldest first; empty for a new file.
    pub records: Vec<Record>,
    /// A torn tail that was cut off on open, if there was one.
    pub repaired: Option<TornTail>,
}

impl OpenTranscript {
    /// The worker events of every stored segment, in order.
    pub fn events(&self) -> Vec<WorkerEvent> {
        self.records
            .iter()
            .filter_map(|r| match r {
                Record::Event { event } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    /// The owner the transcript names, if it names one.
    pub fn owner(&self) -> Option<&Owner> {
        self.records.iter().find_map(|r| match r {
            Record::Owner(name) => Some(name),
            _ => None,
        })
    }

    /// The newest segment: its name, start, and end if it ended.
    pub fn last_segment(&self) -> Option<(&str, &str, Option<&SegmentEnd>)> {
        let mut last = None;
        for record in &self.records {
            match record {
                Record::SegmentStart { segment, start } => {
                    last = Some((segment.as_str(), start.as_str(), None))
                }
                Record::SegmentEnd { segment, end } => {
                    if let Some((name, start, _)) = last
                        && name == segment
                    {
                        last = Some((name, start, Some(end)));
                    }
                }
                _ => {}
            }
        }
        last
    }

    async fn append(&mut self, record: Record) -> Result<()> {
        self.transcript
            .append(record.clone())
            .await
            .context("cannot append to the transcript; it would be short on resume")?;
        self.records.push(record);
        Ok(())
    }

    /// Record a planned segment's start: on a new transcript its owner first,
    /// and an end the plan recovered for the previous segment before it.
    pub async fn begin_segment(&mut self, owner: &Owner, plan: &SegmentPlan) -> Result<()> {
        if self.records.is_empty() {
            self.append(Record::Owner(owner.clone())).await?;
        }
        if let Some((segment, end)) = &plan.recovered {
            self.end_segment(segment, end.clone()).await?;
        }
        self.append(Record::SegmentStart {
            segment: plan.name.clone(),
            start: plan.start.clone(),
        })
        .await
    }

    /// Record how a segment ended.
    pub async fn end_segment(&mut self, segment: &str, end: SegmentEnd) -> Result<()> {
        self.append(Record::SegmentEnd {
            segment: segment.to_owned(),
            end,
        })
        .await
    }
}

/// Open (creating if absent) the transcript at `path` and load what it holds.
pub async fn open_transcript(path: &Path) -> Result<OpenTranscript> {
    let (transcript, repaired) =
        JsonlTranscript::<Record>::open_repairing_torn_tail(path.to_path_buf())
            .await
            .with_context(|| format!("cannot open the transcript {}", path.display()))?;
    let records = transcript
        .load()
        .await
        .with_context(|| format!("cannot read the transcript {}", path.display()))?
        .into_iter()
        .map(|entry| entry.event)
        .collect();
    Ok(OpenTranscript {
        transcript,
        records,
        repaired,
    })
}

/// What the next segment is, decided from the transcript before any worktree
/// is created or seeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentPlan {
    /// The worktree and attempt name, `<worker>-<n>`.
    pub name: String,
    /// The commit the worktree starts from (resolved, so a recorded start
    /// never moves under a later branch update).
    pub start: String,
    /// Whether the loop resumes from the transcript's events.
    pub resume: bool,
    /// An end the transcript missed and the repository proves, recovered for
    /// the previous segment; [`OpenTranscript::begin_segment`] records it first.
    pub recovered: Option<(String, SegmentEnd)>,
}

/// The newest attempt ref and its commit, as the repository has it now.
pub type NewestAttempt = Option<(String, String)>;

/// One attempt ref for a named segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptFacts {
    pub attempt_ref: String,
    pub commit: String,
    /// The commit's first parent: the segment's start, when merge-back made it.
    pub parent: String,
}

/// What [`plan_segment`] reads from the repository. `workspace::GitView` is
/// the real one; nothing here runs git in a worker's tree.
pub trait RepoView {
    /// The newest attempt ref of this worker, with its commit.
    fn newest_attempt(&self) -> Result<NewestAttempt>;
    /// Whether a worktree of this name exists, registered or on disk.
    fn worktree_exists(&self, name: &str) -> Result<bool>;
    /// The attempt ref of one segment, if it exists.
    fn attempt(&self, segment: &str) -> Result<Option<AttemptFacts>>;
    /// A commit-ish resolved to a commit id.
    fn resolve(&self, commitish: &str) -> Result<String>;
}

/// The end a segment without a recorded end provably had, or why none can
/// be proven.
///
/// The end record is written after merge-back commits, so
/// a kill between the two leaves the transcript unended though the work is
/// committed. That state is recognized, not refused: the segment's own
/// attempt ref exists and its commit's parent is the segment's recorded
/// start, which only merge-back's commit of that worktree produces. An
/// absent worktree with no attempt ref is merge-back's empty outcome (it
/// removes the worktree of an attempt that changed nothing). Anything else
/// (a worktree still present with no attempt ref) is a segment that died
/// before its commit, and is refused.
fn recover_end(view: &dyn RepoView, segment: &str, start: &str) -> Result<SegmentEnd> {
    match view.attempt(segment)? {
        Some(facts) if facts.parent == start => Ok(SegmentEnd::Committed {
            attempt_ref: facts.attempt_ref,
            commit: facts.commit,
        }),
        Some(facts) => bail!(
            "segment `{segment}` has no recorded end and its attempt {} = {} does not start \
             from the recorded start {start}; refused",
            facts.attempt_ref,
            facts.commit
        ),
        None if !view.worktree_exists(segment)? => Ok(SegmentEnd::Empty),
        None => bail!(
            "segment `{segment}` never ended: its worktree holds work no commit carries. \
             Inspect it under .kgraf/workspaces/{segment}; it is not resumed over"
        ),
    }
}

/// Decide the next segment, or refuse.
///
/// - A new run needs an empty transcript and no attempts for the worker.
/// - A resume needs a transcript this owner wrote. Its newest segment must
///   have ended (or have an end [`recover_end`] proves); a committed end must
///   still be the newest attempt ref, at the same commit. A refused or
///   uncarried end is refused: its worktree was kept.
/// - The planned name must not exist as a worktree, so creating it never
///   purges a kept or unfinished one.
pub fn plan_segment(
    opened: &OpenTranscript,
    owner: &Owner,
    resume: bool,
    base: &str,
    view: &dyn RepoView,
) -> Result<SegmentPlan> {
    let newest_attempt = view.newest_attempt()?;
    let plan = if !resume {
        if !opened.records.is_empty() {
            bail!(
                "the transcript already holds a run; continue it with --resume, or name a new --events file"
            );
        }
        if let Some((reference, _)) = &newest_attempt {
            bail!(
                "worker `{}` has an attempt from an earlier run ({reference}); resume it with \
                 --resume, or delete its refs to start over",
                owner.worker
            );
        }
        SegmentPlan {
            name: format!("{}-1", owner.worker),
            start: view.resolve(base)?,
            resume: false,
            recovered: None,
        }
    } else {
        let Some(stored) = opened.owner() else {
            bail!("the transcript names no owner; it cannot be resumed");
        };
        if stored != owner {
            bail!(
                "the transcript belongs to worker `{}` task `{}` with brief {}; this resume is \
                 worker `{}` task `{}` with brief {}: refused",
                stored.worker,
                stored.task,
                stored.brief_digest,
                owner.worker,
                owner.task,
                owner.brief_digest
            );
        }
        let Some((segment, start, end)) = opened.last_segment() else {
            bail!("the transcript records no segment; it cannot be resumed");
        };
        let (end, recovered) = match end {
            Some(end) => (end.clone(), None),
            None => {
                let end = recover_end(view, segment, start)?;
                (end.clone(), Some((segment.to_owned(), end)))
            }
        };
        let start = match &end {
            SegmentEnd::Refused { reasons, kept } => bail!(
                "segment `{segment}` was refused by merge-back ({reasons}); its worktree is kept \
                 at {kept} for inspection"
            ),
            SegmentEnd::NotCarried { error, kept } => bail!(
                "segment `{segment}` was not committed ({error}); its worktree is kept at {kept}"
            ),
            SegmentEnd::Committed {
                attempt_ref,
                commit,
            } => {
                if newest_attempt.as_ref() != Some(&(attempt_ref.clone(), commit.clone())) {
                    bail!(
                        "the transcript's last segment ended at {attempt_ref} = {commit}, and the \
                         repository's newest attempt for this worker is {:?}; refused",
                        newest_attempt
                    );
                }
                commit.clone()
            }
            SegmentEnd::Empty => start.to_owned(),
        };
        let n: u64 = segment
            .rsplit_once('-')
            .and_then(|(_, n)| n.parse().ok())
            .with_context(|| format!("segment name `{segment}` has no number"))?;
        SegmentPlan {
            name: format!("{}-{}", owner.worker, n + 1),
            start,
            resume: true,
            recovered,
        }
    };
    if view.worktree_exists(&plan.name)? {
        bail!(
            "a worktree named `{}` already exists; it is never replaced. Inspect and remove it first",
            plan.name
        );
    }
    Ok(plan)
}

/// When [`run_worker`] cancels the worker on its own (Ctrl-C is separate).
pub enum CancelWhen {
    /// This long after the worker starts (`--cancel-after-ms`). Wall-clock, so
    /// where in the run it lands depends on how fast the run goes.
    After(Duration),
    /// Right after the first event the predicate accepts has been handed to
    /// `on_event` (and stored, unless it is a delta the transcript settler
    /// holds; see [`run_worker`]).
    /// Lands at a known point of the run whatever the machine's load; a fixed
    /// delay under load cancelled a test's worker a round early.
    OnEvent(Box<dyn FnMut(&WorkerEvent) -> bool + Send>),
}

impl std::fmt::Debug for CancelWhen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::After(d) => f.debug_tuple("After").field(d).finish(),
            Self::OnEvent(_) => f.write_str("OnEvent(..)"),
        }
    }
}

/// Run `brief` to its end, appending every event to the transcript before
/// handing it to `on_event`, through a [`TranscriptSettler`] (decision 0071):
/// a round's streamed deltas reach `on_event` live but are written only
/// as the round's settled message, or as one whole-scrubbed partial on a
/// cancel. When `resume` is true the loop resumes from the
/// transcript's earlier events. `cancel` cancels the worker at a time or an
/// event; `cancel_on_interrupt` cancels it on Ctrl-C, so the caller still
/// commits its work.
pub async fn run_worker(
    env: Arc<WorkerEnv>,
    brief: WorkerBrief,
    opened: &mut OpenTranscript,
    resume: bool,
    cancel: Option<CancelWhen>,
    cancel_on_interrupt: bool,
    mut on_event: impl FnMut(&WorkerEvent),
) -> Result<WorkerResult> {
    let resume_from = resume.then(|| opened.events());
    let mut handle = spawn_worker(
        env,
        brief,
        SpawnOptions {
            resume_from,
            ..SpawnOptions::default()
        },
    )
    .await
    .context("cannot start the worker")?;
    let (mut cancel_at, mut cancel_on) = match cancel {
        Some(CancelWhen::After(after)) => (Some(tokio::time::Instant::now() + after), None),
        Some(CancelWhen::OnEvent(predicate)) => (None, Some(predicate)),
        None => (None, None),
    };
    let interrupt = async {
        if cancel_on_interrupt {
            // An error here means no handler could be installed; never cancel.
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(interrupt);
    let mut interrupted = false;
    let mut settler = TranscriptSettler::new();
    loop {
        let next = tokio::select! {
            () = async {
                match cancel_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                handle.cancel();
                cancel_at = None;
                continue;
            }
            () = &mut interrupt, if !interrupted => {
                eprintln!("\n[interrupt: cancelling the worker; its work is still committed]");
                handle.cancel();
                interrupted = true;
                continue;
            }
            event = handle.next_event() => event,
        };
        let Some(event) = next else { break };
        // Stored before it is shown, except a streamed delta (decision 0071):
        // the settler holds a round's deltas and writes the round's
        // settled message instead, or its partial text scrubbed whole on a
        // cancel, so a secret split across two deltas is never on disk. A
        // resume drops an unsettled round's text anyway, so every event a
        // resume reads is still stored before it is shown.
        for settled in settler.push(event.clone()) {
            opened.append(Record::Event { event: settled }).await?;
        }
        on_event(&event);
        if cancel_on.as_mut().is_some_and(|accepts| accepts(&event)) {
            handle.cancel();
            cancel_on = None;
        }
    }
    for settled in settler.finish() {
        opened.append(Record::Event { event: settled }).await?;
    }
    handle
        .result()
        .await
        .context("the worker ended without a result")
}
