//! The host's errors.
//!
//! [`Error`] is what building or starting a run returns. Its messages are the ones
//! `kgraf orchestrate` printed before the extraction, word for word, so the
//! command's output did not change. A failed step keeps its cause as a source, and
//! the alternate form (`{:#}`) prints the whole chain the way `anyhow` does, so a
//! caller that shows `{e:#}` (the CLI's sweep line, the shell's run box) loses
//! nothing.
//!
//! [`NotReady`] is the typed answer of the readiness checks
//! ([`crate::RunConfig::check_configured`], [`crate::Host::check_ready`]): the reason a
//! scheduled or interactive run cannot start, before anything is built.

use kgraf_orchestrator::{OrchestratorError, ReadinessFailure};
use std::path::PathBuf;

/// Any error a step can fail with. `anyhow::Error` converts into it too, which is
/// how the `worker` module's errors keep their chain.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a run could not be built, started or finished.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configuration or the request is invalid; the message names the setting.
    #[error("{0}")]
    Invalid(String),
    /// A readiness check failed (typed; see [`NotReady`]).
    #[error(transparent)]
    NotReady(#[from] NotReady),
    /// A setting is wrong, with its cause kept (an invalid `worker.json`, no
    /// commit identity, an invalid `package.json`, a provider the catalog
    /// cannot configure): the same configuration fails the same way again.
    /// Prints like [`Error::Step`]; differs in that it is not transient.
    #[error(transparent)]
    Settings(Chained),
    /// A route's credential is not in the environment at build time. Prints
    /// like [`Error::Step`]; a scheduler disables the schedule with a
    /// missing-credential reason.
    #[error(transparent)]
    Credential(Chained),
    /// The orchestrator refused or ended the run.
    #[error(transparent)]
    Orchestrator(#[from] OrchestratorError),
    /// A step failed; the message says which, and the cause is its source.
    #[error(transparent)]
    Step(Chained),
    /// The project's dependencies are not ready offline and the run may not
    /// fetch them, or the fetch failed (decision 0087). A missing answer
    /// ([`crate::deps::DepsError::consent_question`]) is what a caller that
    /// can ask a person asks about, then starts again.
    #[error(transparent)]
    Dependencies(#[from] crate::deps::DepsError),
}

impl Error {
    /// A step that failed with no cause to keep.
    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Error::Step(Chained {
            context: Some(message.into()),
            inner: None,
        })
    }

    /// This error as the cause of a step described by `context`.
    pub(crate) fn within(self, context: impl Into<String>) -> Self {
        Error::Step(Chained {
            context: Some(context.into()),
            inner: Some(Box::new(self)),
        })
    }

    /// A cause passed through unchanged: display and source are its own.
    pub(crate) fn wrap(e: impl Into<BoxError>) -> Self {
        Error::Step(Chained {
            context: None,
            inner: Some(e.into()),
        })
    }
}

/// A step's message and its cause, displayed like an `anyhow` context: the
/// message alone, or with `{:#}` the message and every cause, joined by `: `.
#[derive(Debug)]
pub struct Chained {
    context: Option<String>,
    inner: Option<BoxError>,
}

impl Chained {
    /// A cause with no message of its own: displays as the cause.
    pub(crate) fn bare(inner: BoxError) -> Self {
        Self {
            context: None,
            inner: Some(inner),
        }
    }
}

impl std::fmt::Display for Chained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut next: Option<&(dyn std::error::Error + 'static)> = match &self.context {
            Some(c) => {
                f.write_str(c)?;
                self.inner.as_deref().map(|e| e as _)
            }
            None => match &self.inner {
                Some(e) => {
                    write!(f, "{e}")?;
                    e.source()
                }
                None => None,
            },
        };
        if f.alternate() {
            while let Some(e) = next {
                write!(f, ": {e}")?;
                next = e.source();
            }
        }
        Ok(())
    }
}

impl std::error::Error for Chained {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match (&self.context, &self.inner) {
            (Some(_), Some(e)) => Some(e.as_ref()),
            (None, Some(e)) => e.source(),
            _ => None,
        }
    }
}

/// `anyhow::Context` for [`Error`]: a step's message with the cause kept.
pub(crate) trait ResultExt<T> {
    /// The cause, under `context`.
    fn step(self, context: impl Into<String>) -> Result<T>;
    /// The cause, under a message built only on failure.
    fn with_step(self, context: impl FnOnce() -> String) -> Result<T>;
    /// The cause as it is (a bare `?` under `anyhow`).
    fn lift(self) -> Result<T>;
    /// The cause as it is, as a settings failure ([`Error::Settings`]).
    fn settings(self) -> Result<T>;
    /// The cause under `context`, as a settings failure.
    fn with_settings(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E: Into<BoxError>> ResultExt<T> for std::result::Result<T, E> {
    fn step(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|e| {
            Error::Step(Chained {
                context: Some(context.into()),
                inner: Some(e.into()),
            })
        })
    }

    fn with_step(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| {
            Error::Step(Chained {
                context: Some(context()),
                inner: Some(e.into()),
            })
        })
    }

    fn lift(self) -> Result<T> {
        self.map_err(Error::wrap)
    }

    fn settings(self) -> Result<T> {
        self.map_err(|e| {
            Error::Settings(Chained {
                context: None,
                inner: Some(e.into()),
            })
        })
    }

    fn with_settings(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| {
            Error::Settings(Chained {
                context: Some(context()),
                inner: Some(e.into()),
            })
        })
    }
}

impl Error {
    /// Whether the same configuration can get past this later: a failed step
    /// (git, IO, an MCP server, a time bound). A settings failure, a readiness
    /// failure and an orchestrator refusal are not.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Step(_) => true,
            Error::Dependencies(e) => e.is_transient(),
            _ => false,
        }
    }

    /// The question a person could answer to let this run start: set only
    /// when the project's dependencies need fetching and nobody has said
    /// whether kGraf may (decision 0087).
    pub fn dependency_question(&self) -> Option<crate::deps::ConsentQuestion> {
        match self {
            Error::Dependencies(e) => e.consent_question(),
            _ => None,
        }
    }
}

impl kgraf_diag::DiagCode for Error {
    fn diag_code(&self) -> &'static str {
        match self {
            Error::NotReady(n) => n.diag_code(),
            _ => kgraf_diag::catch_all_for("kgraf-host").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-host"
    }
}

/// Why a run cannot start as configured, found before anything is built.
///
/// Each maps onto the orchestrator's [`ReadinessFailure`] (`From`), which the
/// scheduled-run seam turns into a claim-table class (0052): a missing credential
/// disables the schedule with that reason; the rest are missing configuration.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotReady {
    /// No worker route is configured.
    #[error("no route is configured: give at least one route:model")]
    NoRoutes,
    /// A route spec is not `route:model`, or a setting is invalid.
    #[error("invalid configuration: {0}")]
    Invalid(String),
    /// The route is not in the provider catalog.
    #[error("unknown route `{route}`: {detail}")]
    UnknownRoute { route: String, detail: String },
    /// The route's credential is not in the environment.
    #[error("missing credential for route `{route}`: {detail}")]
    MissingCredential { route: String, detail: String },
    /// The price table cannot be read, or is not a price table.
    #[error("the price table {} cannot be used: {detail}", path.display())]
    PricesUnreadable { path: PathBuf, detail: String },
    /// Models the price table does not price: spend cannot be reserved.
    #[error("the price table has no price for {}", models.join(", "))]
    MissingPrices { models: Vec<String> },
}

impl kgraf_diag::DiagCode for NotReady {
    fn diag_code(&self) -> &'static str {
        match self {
            NotReady::NoRoutes => "KG-0401",
            NotReady::UnknownRoute { .. } => "KG-0501",
            NotReady::MissingCredential { .. } => "KG-0501",
            NotReady::PricesUnreadable { .. } | NotReady::MissingPrices { .. } => "KG-0501",
            NotReady::Invalid(_) => kgraf_diag::catch_all_for("kgraf-host").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-host"
    }
}

impl From<&NotReady> for ReadinessFailure {
    fn from(n: &NotReady) -> Self {
        match n {
            NotReady::MissingCredential { .. } => {
                ReadinessFailure::MissingCredential(n.to_string())
            }
            _ => ReadinessFailure::MissingConfiguration(n.to_string()),
        }
    }
}

impl From<NotReady> for ReadinessFailure {
    fn from(n: NotReady) -> Self {
        (&n).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch, on both `Error` and `NotReady`, resolves to
    /// a real glossary entry.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let not_ready: Vec<NotReady> = vec![
            NotReady::NoRoutes,
            NotReady::UnknownRoute {
                route: "x".into(),
                detail: "x".into(),
            },
            NotReady::MissingCredential {
                route: "x".into(),
                detail: "x".into(),
            },
            NotReady::PricesUnreadable {
                path: "x".into(),
                detail: "x".into(),
            },
            NotReady::MissingPrices {
                models: vec!["x".into()],
            },
            // Not named in `NotReady::diag_code`'s match: the catch-all arm.
            NotReady::Invalid("x".into()),
        ];
        for n in &not_ready {
            let code = n.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{n:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
        let errors: Vec<Error> = vec![
            Error::NotReady(NotReady::NoRoutes),
            // Not named in `Error::diag_code`'s match: the catch-all arm.
            Error::Invalid("x".into()),
        ];
        for e in &errors {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    /// The chain prints as `anyhow` printed it: the message alone, or every cause
    /// with `{:#}`; and a wrapped cause is transparent.
    #[test]
    fn a_step_prints_its_chain_like_anyhow() {
        let e = Err::<(), _>(Outer(Inner)).step("cannot open").unwrap_err();
        assert_eq!(e.to_string(), "cannot open");
        assert_eq!(format!("{e:#}"), "cannot open: outer: inner");
        let a = anyhow::Error::from(Err::<(), _>(Outer(Inner)).step("cannot open").unwrap_err());
        assert_eq!(format!("{a:#}"), "cannot open: outer: inner");
        let bare = Err::<(), _>(Outer(Inner)).lift().unwrap_err();
        assert_eq!(bare.to_string(), "outer");
        assert_eq!(format!("{bare:#}"), "outer: inner");
        let from_anyhow = Err::<(), _>(anyhow::anyhow!("root").context("mid"))
            .step("top")
            .unwrap_err();
        assert_eq!(format!("{from_anyhow:#}"), "top: mid: root");
    }

    #[test]
    fn a_missing_credential_disables_and_the_rest_are_configuration() {
        let c = NotReady::MissingCredential {
            route: "kimi".into(),
            detail: "set KGRAF_KIMI_API_KEY".into(),
        };
        assert!(matches!(
            ReadinessFailure::from(&c),
            ReadinessFailure::MissingCredential(_)
        ));
        for n in [
            NotReady::NoRoutes,
            NotReady::MissingPrices {
                models: vec!["m".into()],
            },
            NotReady::UnknownRoute {
                route: "x".into(),
                detail: "unknown".into(),
            },
        ] {
            assert!(matches!(
                ReadinessFailure::from(&n),
                ReadinessFailure::MissingConfiguration(_)
            ));
        }
    }
}
