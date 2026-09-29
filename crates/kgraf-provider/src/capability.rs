//! Opt-in capability traits.
//!
//! A capability is a **separate small interface**, present or absent, never a
//! boolean on a wide one (decision 0006). The distinction matters because a
//! boolean can be wrong and a missing implementation cannot: a provider with no
//! cache-control field in its schema at all must not be reachable through a
//! cache-control call path, and the way to guarantee that is for the call path not
//! to exist for it.
//!
//! Rust has no runtime type assertion over `dyn Trait`, so discovery is an
//! accessor returning [`Capabilities`] rather than a downcast. The property is the
//! same: asking is how you find out, and `None` is an honest answer.

use crate::accounting::CallToken;
use crate::error::ProviderError;
use crate::message::{Content, Request, Response, StopReason};
use kgraf_core::TokenUsage;

/// What a provider offers beyond the mandatory trait.
///
/// Every field is `None` unless the adapter opted in.
#[derive(Clone, Copy)]
pub struct Capabilities<'a> {
    pub streaming: Option<&'a dyn Streaming>,
    pub cache_control: Option<&'a dyn CacheControl>,
    pub reasoning: Option<&'a dyn Reasoning>,
}

impl<'a> Capabilities<'a> {
    /// A provider offering nothing beyond the mandatory trait.
    pub fn none() -> Self {
        Self {
            streaming: None,
            cache_control: None,
            reasoning: None,
        }
    }

    /// Declare streaming.
    pub fn with_streaming(mut self, streaming: &'a dyn Streaming) -> Self {
        self.streaming = Some(streaming);
        self
    }

    /// Declare cache control.
    pub fn with_cache_control(mut self, cache_control: &'a dyn CacheControl) -> Self {
        self.cache_control = Some(cache_control);
        self
    }

    /// Declare reasoning control.
    pub fn with_reasoning(mut self, reasoning: &'a dyn Reasoning) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Names of the capabilities present, for display and for the ledger.
    pub fn names(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.streaming.is_some() {
            out.push("streaming");
        }
        if self.cache_control.is_some() {
            out.push("cache_control");
        }
        if self.reasoning.is_some() {
            out.push("reasoning");
        }
        out
    }
}

/// One event from a streaming completion.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// The model the provider says is answering, sent once at the start, so the
    /// model-echo assertion can fire as early as the protocol allows.
    Started {
        model: String,
        request_id: Option<String>,
    },
    /// A fragment of output.
    Delta { content: Content },
    /// A cumulative usage report.
    ///
    /// **Cumulative, not incremental**: Anthropic's streaming usage arrives across
    /// two events and the later one restates the total, so a consumer takes the
    /// last value per field and never sums. A widely repeated belief that the delta
    /// carries only output tokens is true of the legacy examples and false of the
    /// current schema, which is why every field is merged.
    Usage { usage: TokenUsage },
    /// The stream finished cleanly.
    Finished { stop_reason: StopReason },
}

/// A provider that can stream.
#[async_trait::async_trait]
pub trait Streaming: Send + Sync {
    /// Stream a completion, delivering each event to `sink`.
    ///
    /// The assembled response is returned, so a caller that only wants the end
    /// state does not have to reassemble it. A stream that ends without a usage
    /// event returns [`ProviderError::UsageMissing`]: one provider states
    /// explicitly that an interrupted stream may never deliver its usage chunk, and
    /// a call that billed without being counted violates the accounting rule, so it must be
    /// surfaced rather than dropped.
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError>;
}

/// A provider that honors prompt-cache breakpoints.
///
/// Implemented only where the provider genuinely honors them. Cache read and write
/// are counted separately by construction in the usage normal form, because all
/// three reference implementations skipped that and it is where the cost promise is
/// actually kept.
pub trait CacheControl: Send + Sync {
    /// The most breakpoints this provider accepts in one request.
    fn max_breakpoints(&self) -> usize;

    /// The shortest prefix this provider will cache, in tokens, where it publishes
    /// one. `None` means unknown, which is a different claim from zero.
    fn minimum_cacheable_tokens(&self) -> Option<u32>;
}

/// A provider exposing a reasoning-effort control.
pub trait Reasoning: Send + Sync {
    /// Whether reasoning tokens are reported separately from text output.
    ///
    /// Where they are not, the usage form's `reasoning_output` stays zero and those
    /// tokens land in `text_output`. That is a limitation of the protocol, recorded
    /// as one rather than guessed at.
    fn reports_reasoning_tokens(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cache;
    impl CacheControl for Cache {
        fn max_breakpoints(&self) -> usize {
            4
        }
        fn minimum_cacheable_tokens(&self) -> Option<u32> {
            Some(1024)
        }
    }

    #[test]
    fn a_provider_without_a_capability_reports_none_rather_than_a_stub() {
        let capabilities = Capabilities::none();
        assert!(capabilities.cache_control.is_none());
        assert!(capabilities.names().is_empty());
    }

    #[test]
    fn a_declared_capability_is_discoverable_by_asking() {
        let cache = Cache;
        let capabilities = Capabilities::none().with_cache_control(&cache);
        let found = capabilities.cache_control.expect("declared");
        assert_eq!(found.max_breakpoints(), 4);
        assert_eq!(capabilities.names(), vec!["cache_control"]);
    }

    #[test]
    fn an_unknown_minimum_prefix_is_distinct_from_zero() {
        // A stale published minimum-cacheable-prefix table was one of the reasons
        // decision 0006 rejected a third-party aggregator. `None` says "not
        // known", which a caller can act on; `0` would say "always cacheable".
        struct Unknown;
        impl CacheControl for Unknown {
            fn max_breakpoints(&self) -> usize {
                1
            }
            fn minimum_cacheable_tokens(&self) -> Option<u32> {
                None
            }
        }
        assert_eq!(Unknown.minimum_cacheable_tokens(), None);
    }
}
