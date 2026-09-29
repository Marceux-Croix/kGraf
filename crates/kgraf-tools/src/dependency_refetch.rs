//! The seam through which a sandboxed build that could not get a dependency
//! asks the host to fetch it (decision 0087).
//!
//! A sandboxed build runs with the network off and cannot write the cargo
//! cache (decision 0084). When its output shows a dependency that is not
//! ready, the shell tool calls the bound [`DependencyRefetch`] once for that
//! command. The host decides, from the user's per-project consent, whether
//! to fetch; it runs the fetch itself, outside the sandbox, and never on the
//! worktree's own files (see `kgraf-host`'s `deps` module). This crate only
//! carries the answer back into the tool result.

use std::path::Path;

/// What the host did about a dependency a sandboxed build could not get.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefetchOutcome {
    /// The host fetched the worktree's dependencies; the command can run again.
    Fetched { summary: String },
    /// The user has not allowed fetching for this project (or declined it for
    /// this run); `instruction` says what would allow it.
    NotAllowed { instruction: String },
    /// The host tried and could not make the dependencies ready.
    Failed { reason: String },
}

/// Fetches a worktree's missing dependencies on the host, with the user's
/// consent. Called from the shell tool after a failed build.
#[async_trait::async_trait]
pub trait DependencyRefetch: Send + Sync {
    /// `worktree` is the root of the workspace the command ran in.
    async fn refetch(&self, worktree: &Path) -> RefetchOutcome;
}

/// The line the tool result gets for `outcome`, after the build's own output.
pub fn refetch_line(outcome: &RefetchOutcome) -> String {
    match outcome {
        RefetchOutcome::Fetched { summary } => format!(
            "[kgraf] A dependency was not ready in the local cargo cache. kGraf fetched the \
             worktree's dependencies on the host, with the user's consent for this project \
             (decision 0087): {summary}. Run the command again."
        ),
        RefetchOutcome::NotAllowed { instruction } => format!(
            "[kgraf] A dependency is not ready in the local cargo cache, and kGraf may not fetch \
             it: {instruction}"
        ),
        RefetchOutcome::Failed { reason } => format!(
            "[kgraf] A dependency is not ready in the local cargo cache, and kGraf could not \
             fetch it: {reason}"
        ),
    }
}
