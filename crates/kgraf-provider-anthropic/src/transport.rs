//! An alternative transport for the Anthropic Messages body.
//!
//! Some hosts serve the Messages body at a per-model URL instead of `POST
//! /messages`: the model name sits in the path, the request body carries a
//! version string in place of the `model` field, and the caller authenticates
//! with a short-lived bearer token instead of an API key. The body, the
//! streaming events, the usage object and the stop reasons are the same shape,
//! so the adapter serves them through this option rather than a second copy of
//! the parser.
//!
//! The option carries no vendor knowledge. The caller supplies the URL root,
//! the two path verbs, the body version string and the token source.

use std::sync::Arc;

use kgraf_provider::ProviderError;

/// A source of bearer tokens.
///
/// Implementations own token caching and refresh; the adapter asks once per
/// request. A failure must be a [`ProviderError`] that names no secret. Return
/// [`ProviderError::Unsent`] when no request was made, so the ledger row
/// closes as unbilled.
#[async_trait::async_trait]
pub trait BearerSource: Send + Sync {
    /// A currently valid token, without the `Bearer ` prefix.
    async fn bearer(&self) -> Result<String, ProviderError>;

    /// The project to bill and meter the request against, sent as
    /// `x-goog-user-project`, or `None` to send no such header. Only a
    /// credential that belongs to a user needs one.
    fn quota_project(&self) -> Option<String> {
        None
    }
}

/// Serve the Messages body at `{model_root}/{model}:{verb}` with bearer
/// authentication.
#[derive(Clone)]
pub struct ModelInUrlTransport {
    name: &'static str,
    model_root: String,
    complete_verb: &'static str,
    stream_verb: &'static str,
    body_version: String,
    auth: Arc<dyn BearerSource>,
}

impl ModelInUrlTransport {
    /// A transport.
    ///
    /// - `name`: the adapter name reported to the ledger.
    /// - `model_root`: the URL up to and excluding the model segment.
    /// - `complete_verb` and `stream_verb`: the path verbs after the colon.
    /// - `body_version`: the value of the body's `anthropic_version` field.
    pub fn new(
        name: &'static str,
        model_root: impl Into<String>,
        complete_verb: &'static str,
        stream_verb: &'static str,
        body_version: impl Into<String>,
        auth: Arc<dyn BearerSource>,
    ) -> Self {
        Self {
            name,
            model_root: model_root.into().trim_end_matches('/').to_owned(),
            complete_verb,
            stream_verb,
            body_version: body_version.into(),
            auth,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn body_version(&self) -> &str {
        &self.body_version
    }

    pub(crate) fn url(&self, model: &str, stream: bool) -> String {
        let verb = if stream {
            self.stream_verb
        } else {
            self.complete_verb
        };
        format!("{}/{model}:{verb}", self.model_root)
    }

    pub(crate) async fn bearer(&self) -> Result<String, ProviderError> {
        self.auth.bearer().await
    }

    pub(crate) fn quota_project(&self) -> Option<String> {
        self.auth.quota_project()
    }

    /// Whether `returned` names the requested model.
    ///
    /// The response body is the Messages response, whose `model` is an Anthropic
    /// model id. The published guide does not state how that id relates to the
    /// id in the URL, so three forms are accepted and every other difference is
    /// a substitution the caller refuses:
    ///
    /// - the id as requested;
    /// - `name@version` answered as `name-version`, Anthropic's spelling of a
    ///   dated id;
    /// - a bare `name` answered as `name-` plus an eight-digit date, the dated
    ///   id an alias resolves to.
    ///
    /// <https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/claude/use-claude>
    /// lists both the `name` and `name@version` forms of a request.
    pub(crate) fn echoes(&self, requested: &str, returned: &str) -> bool {
        if requested == returned {
            return true;
        }
        if requested.contains('@') {
            return requested.replace('@', "-") == returned;
        }
        returned
            .strip_prefix(requested)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
    }
}

impl std::fmt::Debug for ModelInUrlTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token source is deliberately not printed.
        f.debug_struct("ModelInUrlTransport")
            .field("name", &self.name)
            .field("model_root", &self.model_root)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;

    #[async_trait::async_trait]
    impl BearerSource for Fixed {
        async fn bearer(&self) -> Result<String, ProviderError> {
            Ok("token".to_owned())
        }
    }

    fn transport() -> ModelInUrlTransport {
        ModelInUrlTransport::new(
            "test-transport",
            "https://host.invalid/models/",
            "complete",
            "stream",
            "v-test",
            Arc::new(Fixed),
        )
    }

    #[test]
    fn the_url_carries_the_model_and_the_verb() {
        let t = transport();
        assert_eq!(
            t.url("m@1", false),
            "https://host.invalid/models/m@1:complete"
        );
        assert_eq!(t.url("m@1", true), "https://host.invalid/models/m@1:stream");
        assert_eq!(t.body_version(), "v-test");
        assert_eq!(t.name(), "test-transport");
    }

    #[test]
    fn only_the_documented_spellings_of_the_same_model_are_an_echo() {
        let t = transport();
        assert!(t.echoes("m@20250101", "m-20250101"));
        assert!(t.echoes("m", "m"));
        assert!(t.echoes("m@20250101", "m@20250101"));
        // An alias resolved to a dated id.
        assert!(t.echoes("m", "m-20250101"));
        assert!(!t.echoes("m@20250101", "m-20250102"));
        assert!(!t.echoes("m@20250101", "other-20250101"));
        // A different model that merely starts with the requested name.
        assert!(!t.echoes("m", "m-mini"));
        assert!(!t.echoes("m", "m-2025"));
        assert!(!t.echoes("m", "m-2025010a"));
        // A dated request answered by the bare name is a different claim.
        assert!(!t.echoes("m@20250101", "m"));
    }
}
