//! Readiness before capacity.
//!
//! Ported from LibreChat's scheduled-run admission rule,
//! `packages/api/src/schedules/README.md` and `capacity.ts`, commit
//! 7b2362d7a7c6148b84850924dc7fa5fc43307923, MIT (license read at the source
//! 2026-09-26; see `third_party/librechat/UPSTREAM`): before a run takes a slot,
//! verify the credential, MCP reachability, and a **complete** tool discovery
//! containing every tool the run selected; "a slow or failed readiness check never
//! occupies generation capacity". Failures are typed, and a credential-store outage
//! is transient, never proof the credential is missing.
//!
//! kGraf's dispatch order is readiness, then admission and budget, then leases,
//! then run, so a worker that fails readiness never holds a lease another worker is
//! waiting on (0048).

use kgraf_schedule::{DisableReason, FailureClass};
use std::sync::Arc;

/// Why a route is not ready. The four classes the roadmap names.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadinessFailure {
    #[error("missing credential: {0}")]
    MissingCredential(String),
    #[error("missing configuration: {0}")]
    MissingConfiguration(String),
    #[error("permission revoked: {0}")]
    PermissionRevoked(String),
    #[error("transient: {0}")]
    Transient(String),
}

impl ReadinessFailure {
    /// Whether waiting could fix it.
    pub fn is_transient(&self) -> bool {
        matches!(self, ReadinessFailure::Transient(_))
    }

    /// The scheduling crate's classification (0052): the first three disable a
    /// schedule, the fourth counts toward its threshold.
    pub fn to_failure_class(&self) -> FailureClass {
        match self {
            ReadinessFailure::MissingCredential(d) => {
                FailureClass::Disabling(DisableReason::MissingCredential { detail: d.clone() })
            }
            ReadinessFailure::MissingConfiguration(d) => {
                FailureClass::Disabling(DisableReason::ConfigurationMissing { detail: d.clone() })
            }
            ReadinessFailure::PermissionRevoked(d) => {
                FailureClass::Disabling(DisableReason::PermissionDenied { detail: d.clone() })
            }
            ReadinessFailure::Transient(d) => FailureClass::Transient { detail: d.clone() },
        }
    }
}

/// Checks a route can run before it gets capacity: credentials present, MCP
/// servers reachable. Tool completeness is checked by the orchestrator against
/// the route's registry, since it needs the brief's tool list.
#[async_trait::async_trait]
pub trait ReadinessProbe: Send + Sync {
    async fn check(&self) -> Result<(), ReadinessFailure>;
}

/// A probe that always passes: for a route whose provider was already built with
/// its credential (building one without a credential fails) and has no MCP
/// servers.
#[derive(Clone, Copy, Debug, Default)]
pub struct AlwaysReady;

#[async_trait::async_trait]
impl ReadinessProbe for AlwaysReady {
    async fn check(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
}

/// A probe from a closure (for the CLI's environment-variable credential check).
pub struct FnProbe(pub Arc<dyn Fn() -> Result<(), ReadinessFailure> + Send + Sync>);

#[async_trait::async_trait]
impl ReadinessProbe for FnProbe {
    async fn check(&self) -> Result<(), ReadinessFailure> {
        (self.0)()
    }
}

/// Every tool the brief names must be in the route's discovered set.
pub fn check_tools<'a>(
    available: impl Iterator<Item = &'a str>,
    needed: &[String],
) -> Result<(), ReadinessFailure> {
    let have: std::collections::BTreeSet<&str> = available.collect();
    let missing: Vec<&str> = needed
        .iter()
        .map(String::as_str)
        .filter(|t| !have.contains(t))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ReadinessFailure::MissingConfiguration(format!(
            "tool discovery lacks {missing:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_onto_the_schedulers_classes() {
        assert!(
            ReadinessFailure::MissingCredential("KIMI_API_KEY".into())
                .to_failure_class()
                .disable_reason()
                .is_some()
        );
        assert!(
            ReadinessFailure::Transient("keychain locked".into())
                .to_failure_class()
                .disable_reason()
                .is_none()
        );
        assert!(ReadinessFailure::Transient("x".into()).is_transient());
        assert!(!ReadinessFailure::PermissionRevoked("x".into()).is_transient());
    }

    #[test]
    fn an_incomplete_tool_discovery_is_a_configuration_failure() {
        let have = ["read_file", "write_file"];
        assert!(check_tools(have.iter().copied(), &["write_file".into()]).is_ok());
        let err = check_tools(have.iter().copied(), &["shell".into()]).expect_err("missing");
        assert!(
            matches!(err, ReadinessFailure::MissingConfiguration(ref d) if d.contains("shell"))
        );
    }
}
