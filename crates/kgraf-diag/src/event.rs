//! The event vocabulary this crate records. See the crate root for the file
//! format and the standing rule this satisfies (decision 0069).

use serde::{Deserialize, Serialize};

/// One line of the diagnostics file: a timestamp plus one event. `#[serde(flatten)]`
/// puts `ts_ms` next to the event's own tagged fields, so a line reads as one flat
/// JSON object (`{"ts_ms":...,"kind":"launch"}`) rather than a nested one; that is
/// what keeps a hand read (`cat`, `jq`) as easy as `kgraf diag show`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub ts_ms: i64,
    #[serde(flatten)]
    pub event: DiagEvent,
}

impl Record {
    pub fn now(event: DiagEvent) -> Self {
        Self {
            ts_ms: now_ms(),
            event,
        }
    }
}

/// Milliseconds since the Unix epoch. Never panics: a clock before 1970 (the
/// only way `duration_since` fails) reports 0 rather than taking down the
/// caller's hot path over a diagnostics timestamp (standing rule: fail loud
/// only where the failure is load-bearing; a lost timestamp is not).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Basic usage counts and errors: exactly the two content classes the
/// diagnostics file holds, nothing else. No secrets, no file contents, no prompt or
/// completion text ever appears in a variant here; a caller that has such text
/// must summarize or scrub it before it reaches `record` (see `scrub.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagEvent {
    /// The app or CLI started. `feature` merges in the launch's feature use
    /// (`kgraf-cli`'s subcommand) when the caller has one at launch time, so
    /// a CLI invocation writes one line instead of two, not one line for the
    /// launch and another for its feature record. `#[serde(default)]`
    /// so an older file's bare `{"kind":"launch"}` line (written before this
    /// field existed) still parses, as `None`.
    Launch {
        #[serde(default)]
        feature: Option<String>,
    },
    /// A run began. `run` is the run id already exposed on the graph and in
    /// the ledger, not a path.
    RunStarted { run: String },
    /// A run ended, successfully or not.
    RunFinished { run: String, ok: bool },
    /// A worker attempt was admitted on a provider route. `provider` is the
    /// catalog route id (for example `kimi`), `model` the model name on it;
    /// both are already public in the routing catalog, never a credential.
    WorkerSpawned { provider: String, model: String },
    /// A named feature path was used (a CLI subcommand, a shell action). Free
    /// text is not allowed here; callers pass one of a small fixed set of
    /// names so this stays a count, not a log. `String`, not `&'static str`:
    /// `#[serde(flatten)]` (used on `Record` above) buffers through a
    /// self-describing map while deserializing, which cannot hand back
    /// borrowed data, so every string field on this enum has to be owned to
    /// round-trip through `Deserialize`; callers still typically pass a
    /// `&'static str` literal, which converts for free.
    FeatureUsed { feature: String },
    /// An error, keyed to the glossary in `glossary.rs`.
    Error {
        /// A stable `KG-nnnn` code from the glossary.
        code: String,
        /// The crate or component that raised it (for example `kgraf-host`).
        component: String,
        /// A short, scrubbed context string: no secrets, no file contents,
        /// no prompt or completion text, and any path already put through
        /// `scrub::scrub_paths`.
        context: String,
    },
    /// kGraf fetched a project's dependencies from a package registry, with
    /// the user's consent for that project (decision 0087). A network event
    /// with no tokens, recorded so it is never invisible. `registry` is the
    /// registry's public name (`crates.io`), `trigger` is `run-start` or
    /// `worker`; no path, no package names.
    RegistryFetch {
        registry: String,
        trigger: String,
        ok: bool,
        downloaded: u64,
    },
    /// `count` events were dropped by a full recording channel since the last
    /// time this was reported (`writer.rs`'s `DROPPED` counter). Written the
    /// next time a write succeeds, so a gap in the file's timeline shows as
    /// this record rather than looking like nothing happened.
    Dropped { count: u64 },
}

impl DiagEvent {
    /// A launch with no feature to report yet (the desktop shell has no
    /// subcommand concept; a CLI launch prefers [`Self::launch_with_feature`]).
    pub fn launch() -> Self {
        Self::Launch { feature: None }
    }

    /// A launch that already knows which feature it is about to run (a CLI
    /// subcommand): one record instead of a separate `Launch` and
    /// `FeatureUsed` pair.
    pub fn launch_with_feature(feature: impl Into<String>) -> Self {
        Self::Launch {
            feature: Some(feature.into()),
        }
    }

    pub fn worker_spawned(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self::WorkerSpawned {
            provider: provider.into(),
            model: model.into(),
        }
    }

    pub fn error(
        code: impl Into<String>,
        component: impl Into<String>,
        context: impl Into<String>,
    ) -> Self {
        Self::Error {
            code: code.into(),
            component: component.into(),
            context: context.into(),
        }
    }
}
