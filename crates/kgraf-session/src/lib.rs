//! Session machinery: skills at run time (0053), tool-approval hooks, and
//! the resumable transcript.
//!
//! - [`approval`]: pre-tool hooks returning allow, ask, deny or rewrite. Defined in
//!   `kgraf-tools` and re-exported here, because approval is bound to execution:
//!   `Tool::call` needs a permit that only an approved call yields, and Rust has no
//!   way to share a private constructor across crates.
//! - [`transcript`]: append neutral events, load them for resume. [`transcript::JsonlTranscript`]
//!   is the durable, file-backed implementation (ported from codex-rs `rollout/`;
//!   `third_party/codex/UPSTREAM`).
//! - [`skills`]: the skill catalog over `kgraf-instructions`' inventory, and the
//!   run-time enforcement of `allowed-tools` and `disable-model-invocation`.
//! - [`reduction`]: decision 0057's non-rewriting context reduction (observation
//!   masking ported from SWE-agent, truncation sizing ported from OpenHands;
//!   `third_party/sweagent/UPSTREAM`, `third_party/openhands/UPSTREAM`).
//! - [`approval_store`]: persisted per-project allow rules ("always allow this
//!   command pattern for this project"), applied as an [`approval::ApprovalHook`]
//!   (ported from goose `permission/`; `third_party/goose/UPSTREAM`).

pub mod approval_store;
pub mod reduction;
pub mod skills;
pub mod transcript;

pub use kgraf_tools::approval;

use kgraf_tools::ToolError;
use std::path::PathBuf;

/// Session failures.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transcript store failed.
    #[error("transcript store failed: {0}")]
    Transcript(String),

    /// No skill has this name.
    #[error("no skill named `{name}`")]
    UnknownSkill { name: String },

    /// Two skills passed to the catalog share a name.
    #[error("skill `{name}` appears twice in the catalog input")]
    DuplicateSkill { name: String },

    /// The model tried to invoke a skill marked `disable-model-invocation`.
    #[error("skill `{name}` may not be invoked by the model (disable-model-invocation)")]
    ModelInvocationDisabled { name: String },

    /// The user tried to invoke a skill marked `user-invocable: false`.
    #[error("skill `{name}` may not be invoked by the user (user-invocable: false)")]
    NotUserInvocable { name: String },

    /// A skill's `allowed-tools` entry cannot be read. The skill is refused rather
    /// than activated with a restriction wider or narrower than its author meant.
    #[error("skill `{skill}` has an unreadable allowed-tools entry `{entry}`")]
    BadAllowedTools { skill: String, entry: String },

    /// A skill's `allowed-tools` has entries that name no tool this worker has.
    /// The skill is refused: its restriction cannot mean what its author wrote,
    /// and a worker under it would lack tools it was told to use. `unknown` lists
    /// the entries as written (with a hint where a known tool does the same job)
    /// and `available` the worker's tools.
    #[error(
        "skill `{skill}` lists tools in allowed-tools that this worker does not have: {}; the tools this worker has are: {}",
        unknown.join(", "),
        available.join(", ")
    )]
    UnknownAllowedTools {
        skill: String,
        unknown: Vec<String>,
        available: Vec<String>,
    },

    /// An active skill's `allowed-tools` does not permit this call.
    #[error("tool `{tool}` is not permitted by skill `{skill}`'s allowed-tools")]
    ToolRefused { tool: String, skill: String },

    /// The project's skill inventory exists and cannot be read. Refused rather
    /// than run with no skills: a restriction the user ingested must not vanish.
    #[error("cannot read the skill inventory: {0}")]
    Inventory(#[from] kgraf_instructions::InstructionsError),

    /// A skill was activated for a worker whose approval chain carries no skill
    /// policy, so its `allowed-tools` would not be enforced.
    #[error(
        "skill `{name}` cannot be activated for worker `{worker}`: no skill policy is attached to its approval chain"
    )]
    NoSkillPolicy { name: String, worker: String },

    /// A resumed transcript activated a skill that has changed since, so its
    /// `allowed-tools` may not be the restriction the worker ran under.
    #[error(
        "skill `{name}` changed since the transcript activated it (version {recorded}, now {current})"
    )]
    SkillVersionChanged {
        name: String,
        recorded: String,
        current: String,
    },

    /// A resumed transcript activated a skill and its result does not say
    /// which version, so the restriction cannot be checked against it.
    #[error("the transcript does not record which version of skill `{name}` it activated")]
    SkillVersionUnrecorded { name: String },

    /// The tool itself failed to run.
    #[error(transparent)]
    Tool(#[from] ToolError),

    /// The transcript file's last line was cut short (a crash mid-write), or a
    /// line that is not the last one failed to parse. Never dropped silently
    /// (fail loud): see [`transcript::JsonlTranscript::open`].
    #[error("transcript {path:?} has a torn line at byte {byte_offset}: {content:?}")]
    TornTranscriptLine {
        path: PathBuf,
        byte_offset: u64,
        content: String,
    },

    /// A non-final transcript line is not valid JSON for the event type. Unlike
    /// a torn last line, this is not attributable to an in-flight write and
    /// signals real corruption.
    #[error("transcript {path:?} line {line} is corrupt: {reason}")]
    CorruptTranscriptLine {
        path: PathBuf,
        line: u64,
        reason: String,
    },

    /// The transcript store's underlying I/O failed.
    #[error("transcript I/O failed for {path:?}: {source}")]
    TranscriptIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The persisted allow-rule store's underlying I/O or (de)serialization failed.
    #[error("approval store {path:?} failed: {reason}")]
    ApprovalStore { path: PathBuf, reason: String },

    /// A transcript write failed and could not be rolled back cleanly (the
    /// `set_len` truncation itself failed). The handle refuses every further
    /// operation rather than risk appending after an unrecoverable partial
    /// write (fail loud).
    #[error("transcript {path:?} is poisoned: a failed write could not be rolled back")]
    TranscriptPoisoned { path: PathBuf },

    /// A parsed entry's `seq` did not equal its position among the entries
    /// read so far. Catches a duplicated or skipped sequence number -- the
    /// on-disk symptom a cancelled, un-rolled-back append would leave -- as a loud read-time failure rather than a silently
    /// accepted resume.
    #[error("transcript {path:?}: entry at position {index} has seq {seq}, expected {index}")]
    TranscriptSeqMismatch { path: PathBuf, index: u64, seq: u64 },

    /// The transcript file is not valid UTF-8. Refused rather than lossily
    /// substituted: a substitution could silently turn a
    /// corrupt byte into a plausible-looking character inside a JSON string.
    #[error("transcript {path:?} is not valid UTF-8 at byte {byte_offset}")]
    InvalidUtf8 { path: PathBuf, byte_offset: u64 },

    /// Another handle already holds this transcript's exclusive lock. Disjoint writes mean this should not arise from
    /// normal operation (one worker owns one worktree), so a refusal here
    /// means something outside that invariant has gone wrong; resuming
    /// silently anyway could interleave two writers' records.
    #[error("transcript {path:?} is already open for writing by another handle")]
    TranscriptLocked { path: PathBuf },

    /// A write for `seq` landed only partially before it was resolved: the
    /// file grew by `written` bytes, not the `expected` length of the record.
    /// Rolled back before this is returned, so the file
    /// itself is clean; the caller must retry the event at `seq`.
    #[error(
        "transcript {path:?}: the write for seq {seq} landed partially \
         ({written} of {expected} bytes); rolled back"
    )]
    TranscriptPartialWrite {
        path: PathBuf,
        seq: u64,
        written: u64,
        expected: u64,
    },
}
