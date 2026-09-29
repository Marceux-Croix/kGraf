//! The mandatory trait, and per-worker configuration.
//!
//! The seam is **small and mandatory, plus separate opt-in capability traits**
//! (decision 0006). Three methods, not thirty, and no capability booleans: a
//! provider either implements [`crate::capability::CacheControl`] or it does not,
//! and a caller discovers which by asking, not by reading a flag that may be
//! wrong.
//!
//! The counterexamples this shape is chosen against are instructive. One
//! reference's provider seam is a single untyped method returning a bare
//! dictionary with no runtime enforcement, too thin to carry usage, cache counts,
//! cancellation, streaming or cost. Another declares one method and hides two
//! more as undeclared duck-typed extras.

use crate::accounting::CallToken;
use crate::error::ProviderError;
use crate::message::{Request, Response};
use kgraf_core::Protocol;

/// One worker's provider configuration.
///
/// Held per worker, not globally: provider, model, credentials and base URL vary
/// per worker within the single process (decision 0006), which is what makes a
/// heterogeneous fleet possible at all.
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    /// The endpoint root, without a trailing slash.
    pub base_url: String,
    /// The credential. Absent where one is required is a configuration error raised
    /// at construction, never an empty result at run time (fail loud).
    ///
    /// Empty is legitimate for a locally hosted endpoint, which is why
    /// [`ProviderConfig::local`] exists: the distinction between "no credential
    /// because none is needed" and "no credential because it was forgotten" is a
    /// declaration the caller makes, not something inferred from an empty string.
    pub api_key: String,
    /// Whether this endpoint needs a credential at all.
    ///
    /// False only for a local endpoint built through [`ProviderConfig::local`].
    pub requires_credential: bool,
    /// Models this configuration may request.
    ///
    /// An adapter asked for a model outside this set refuses **before** the
    /// request goes out, because one provider silently remaps an unknown model
    /// name to its cheap model and returns HTTP 200.
    pub models: Vec<String>,
    /// Whether this endpoint's usage is additive or inclusive.
    ///
    /// Configured per endpoint rather than derived from a vendor name, because one
    /// vendor ships both conventions behind a single API key: the same credential
    /// against its native path and its Anthropic-compatible path gives opposite
    /// semantics.
    pub protocol: Protocol,
    /// Whether this endpoint is known to honor cache control.
    ///
    /// Defaults to false where unknown, because emitting breakpoints at a
    /// provider that drops them costs money silently and gains nothing.
    pub honors_cache_control: bool,
    /// Request timeout.
    pub timeout: std::time::Duration,
}

impl ProviderConfig {
    /// A configuration, refusing an absent credential at construction.
    pub fn new(
        adapter: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        models: Vec<String>,
        protocol: Protocol,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ProviderError::MissingCredential { adapter });
        }
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
            requires_credential: true,
            models,
            protocol,
            honors_cache_control: false,
            timeout: std::time::Duration::from_secs(600),
        })
    }

    /// A configuration for a locally hosted endpoint, which takes no credential.
    ///
    /// Ollama, llama.cpp's server, vLLM and LM Studio all serve an
    /// OpenAI-compatible surface on localhost with no authentication, and requiring
    /// a placeholder key would be a lie that the next reader has to decode.
    ///
    /// This does not weaken the fail-loud rule. That rule is about an absent dependency reading
    /// as success; here the absence is **declared** by choosing this constructor, so
    /// a forgotten key on a remote endpoint still raises in
    /// [`ProviderConfig::new`]. The two cases stay distinguishable, which is the
    /// whole point.
    pub fn local(base_url: impl Into<String>, models: Vec<String>, protocol: Protocol) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: String::new(),
            requires_credential: false,
            models,
            protocol,
            honors_cache_control: false,
            // Local inference on consumer hardware is slow, and a self-hosted model
            // has no rate limit to protect. A timeout tuned for a hosted endpoint
            // would cancel legitimate work.
            timeout: std::time::Duration::from_secs(1800),
        }
    }

    /// The credential to send, or `None` for an endpoint that takes none.
    ///
    /// Adapters must use this rather than reading `api_key` directly, so a local
    /// endpoint is sent no authorization header at all instead of an empty one.
    /// Some servers reject a malformed `Authorization` header outright, which would
    /// present as an authentication failure against a server that has no
    /// authentication.
    pub fn credential(&self) -> Option<&str> {
        if self.requires_credential || !self.api_key.is_empty() {
            Some(&self.api_key)
        } else {
            None
        }
    }

    /// Declare that this endpoint honors cache control.
    pub fn honoring_cache_control(mut self) -> Self {
        self.honors_cache_control = true;
        self
    }

    /// Set the request timeout.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Refuse a model this configuration does not serve.
    pub fn check_model(&self, adapter: &'static str, model: &str) -> Result<(), ProviderError> {
        if self.models.iter().any(|m| m == model) {
            return Ok(());
        }
        Err(ProviderError::ModelUnsupported {
            adapter,
            model: model.to_owned(),
            available: self.models.join(", "),
        })
    }
}

/// The mandatory provider interface.
///
/// [`Provider::complete`] takes a [`CallToken`], and a token can only come from
/// [`crate::accounting::Ledger::begin`]. That is how the accounting rule is enforced: there is
/// no signature that lets a caller reach a provider without the ledger having
/// already recorded the attempt.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// The adapter's stable name, for errors and for the ledger.
    fn adapter(&self) -> &'static str;

    /// Which usage convention this configured endpoint reports in.
    fn protocol(&self) -> Protocol;

    /// Issue a completion.
    ///
    /// Implementations must, in order: refuse a model they do not serve; issue the
    /// request; assert the response model equals the requested model; and
    /// normalize usage under [`Provider::protocol`].
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError>;

    /// Capabilities this adapter offers beyond the mandatory three methods.
    ///
    /// Rust has no runtime type assertion over trait objects, so discovery is an
    /// explicit accessor rather than a downcast. The shape is the same: a small
    /// separate interface per capability, absent rather than stubbed where the
    /// provider does not have it.
    fn capabilities(&self) -> crate::capability::Capabilities<'_> {
        crate::capability::Capabilities::none()
    }
}

/// Assert the response model is the requested model.
///
/// Every adapter calls this. Three providers were confirmed to silently substitute
/// or ignore, all returning HTTP 200, and one of them remaps an unknown model name
/// to its cheap model, which is a correctness bug in a worker pool, not only a
/// billing one.
pub fn assert_model_echo(requested: &str, returned: &str) -> Result<(), ProviderError> {
    if requested == returned {
        return Ok(());
    }
    Err(ProviderError::ModelSubstituted {
        requested: requested.to_owned(),
        returned: returned.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig::new(
            "test",
            "https://example.invalid/v1/",
            "key",
            vec!["good-model".into()],
            Protocol::Additive,
        )
        .expect("config")
    }

    #[test]
    fn an_absent_credential_is_refused_at_construction() {
        // The fail-loud rule: a missing dependency is an error at startup, not an empty result
        // at run time.
        let err = ProviderConfig::new("test", "https://x", "   ", vec![], Protocol::Additive)
            .expect_err("must refuse");
        assert!(matches!(err, ProviderError::MissingCredential { .. }));
    }

    #[test]
    fn a_model_the_adapter_does_not_serve_is_refused_before_the_request() {
        let err = config()
            .check_model("test", "typo-model")
            .expect_err("must refuse");
        match err {
            ProviderError::ModelUnsupported { available, .. } => {
                assert!(
                    available.contains("good-model"),
                    "the error must say what is available"
                );
            }
            other => panic!("wrong error: {other:?}"),
        }
        config().check_model("test", "good-model").expect("served");
    }

    #[test]
    fn a_substituted_model_raises() {
        assert!(assert_model_echo("asked", "asked").is_ok());
        let err = assert_model_echo("asked", "something-cheaper").expect_err("must raise");
        assert!(matches!(err, ProviderError::ModelSubstituted { .. }));
    }

    #[test]
    fn cache_control_defaults_to_not_honored() {
        assert!(!config().honors_cache_control);
        assert!(config().honoring_cache_control().honors_cache_control);
    }

    #[test]
    fn a_local_endpoint_needs_no_credential_and_sends_none() {
        let local = ProviderConfig::local(
            "http://localhost:11434/v1/",
            vec!["some-local-model".into()],
            Protocol::Inclusive,
        );
        assert!(!local.requires_credential);
        assert_eq!(local.credential(), None);
        assert_eq!(local.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn a_forgotten_credential_on_a_remote_endpoint_still_raises() {
        // The two absences must stay distinguishable, or declaring one legitimate
        // would excuse the other.
        assert!(
            ProviderConfig::new(
                "test",
                "https://remote.invalid",
                "",
                vec![],
                Protocol::Inclusive
            )
            .is_err()
        );
        assert_eq!(config().credential(), Some("key"));
    }

    #[test]
    fn a_local_endpoint_gets_a_longer_default_timeout() {
        // Local inference on consumer hardware is slow and has no rate limit to
        // protect; a hosted-endpoint timeout would cancel legitimate work.
        let local = ProviderConfig::local("http://localhost:8080", vec![], Protocol::Inclusive);
        assert!(local.timeout > config().timeout);
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_normalized() {
        assert_eq!(config().base_url, "https://example.invalid/v1");
    }
}
