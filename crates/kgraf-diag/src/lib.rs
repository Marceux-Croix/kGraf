//! Local-only diagnostics (decision 0069).
//!
//! One capped, per-user file records basic usage counts (launches, runs
//! started and finished, workers spawned, the providers and models used by
//! name, and named feature use) and every error, each keyed to a stable
//! `KG-nnnn` glossary code. Nothing here is ever sent anywhere: there is no
//! network code in this crate at all, which is the point, not an oversight
//! (decision 0069: this is diagnostics the user pulls and
//! shares by hand, not telemetry kGraf collects). If a diagnosis is ever
//! needed, the user runs `kgraf diag export` and shares the result however
//! they choose.
//!
//! # Modules
//!
//! - [`event`]: the recorded vocabulary ([`event::DiagEvent`]).
//! - [`scrub`]: turns a path into either a project-relative string or a
//!   stable hash, so a path never carries the user's account name.
//! - [`store`]: the JSONL file format, its roughly 1 MiB cap, and
//!   trim-on-threshold rotation (design rationale in that module's doc).
//! - [`writer`]: the bounded channel and background thread `init`/`record`
//!   use so recording never blocks a caller's hot path and never panics.
//! - [`glossary`]: the `KG-nnnn` table; the single source `docs/ERRORS.md` is
//!   generated from.
//! - [`docs`]: renders the glossary as that Markdown.
//! - [`report`]: `kgraf diag show` (a human summary) and `kgraf diag export`
//!   (a re-scrubbed, shareable copy).
//!
//! # Using this crate
//!
//! A binary entry point calls [`init`] once, with the path
//! `app_dirs`-equivalent logic for that binary resolves (see
//! `kgraf-cli::diag_cmd` and `kgraf-shell`'s startup for the two callers).
//! Every crate downstream of it can then call [`record`] without holding a
//! handle: this mirrors `tracing`'s global-subscriber precedent already used
//! in this workspace (`kgraf-cli/src/logging.rs`), which is the right model
//! for a cross-cutting, always-on concern like this one rather than a value
//! threaded through every call site.

pub mod code;
pub mod docs;
pub mod event;
pub mod glossary;
pub mod key;
pub mod paths;
pub mod report;
pub mod scrub;
pub mod store;
pub mod writer;

pub use code::DiagCode;
pub use event::{DiagEvent, Record};
pub use glossary::{ErrorSpec, GLOSSARY, catch_all_for};
pub use paths::{DISABLE_ENV, PATH_ENV, default_path, resolve_path};
pub use store::{DEFAULT_CAP_BYTES, DiagError};
pub use writer::{DiagWriter, ShutdownGuard, flush_for_test, guard, init, record, shutdown};

/// The file name this crate writes under the caller-resolved data directory.
/// Kept as one constant so the CLI and the shell can never drift onto two
/// different file names for the same per-user diagnostics file (requirement
/// 1: "one per-user local file").
pub const FILE_NAME: &str = "diagnostics.jsonl";
