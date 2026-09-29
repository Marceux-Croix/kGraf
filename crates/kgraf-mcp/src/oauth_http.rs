//! [`BoundedOAuthHttpClient`]: the HTTP client `rmcp`'s OAuth state machine uses
//! for discovery, registration, token exchange and refresh.
//!
//! Adapted from modelcontextprotocol/rust-sdk at
//! 9427a929959e665e0d12e9395f674026baf4bd48 (the `rmcp` 3.4.1 release),
//! `crates/rmcp/src/transport/auth.rs` (`ReqwestOAuthHttpClient` and
//! `default_oauth_http_client`). Apache-2.0; see `third_party/rmcp/`.
//!
//! **Why not `rmcp`'s default.** `AuthorizationManager::new` builds
//! its client from `reqwest::Client::builder()`, which routes through
//! `HTTP_PROXY`/`HTTPS_PROXY`, so discovery, token exchange and refresh (which
//! carry the refresh token and client secret) went to any proxy in the
//! environment. Changes from upstream:
//!
//! - both inner clients call `.no_proxy()`; a proxy is used only when the
//!   server's configuration names one, and then for both;
//! - the redirect-following client follows only same-origin redirects, at most
//!   [`MAX_OAUTH_REDIRECTS`] hops (upstream: the `reqwest` default, 10 hops to any
//!   origin);
//! - the redirect-stopping client returns a 30x as it is; with
//!   `AuthorizationManager::new_with_oauth_http_client` token refresh uses it, so a
//!   30x on refresh is an error, never followed;
//! - bodies are capped at [`MAX_OAUTH_BODY_BYTES`] as upstream does (1 MiB).

use futures_util::StreamExt;
use rmcp::transport::auth::{
    OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
    OAuthHttpRequest,
};
use std::time::Duration;

/// The longest OAuth response body read (upstream's value).
pub const MAX_OAUTH_BODY_BYTES: usize = 1024 * 1024;

/// The most same-origin redirects followed for one OAuth request.
pub const MAX_OAUTH_REDIRECTS: usize = 3;

/// The timeout for one OAuth request when `rmcp` does not suggest one
/// (upstream's value).
const OAUTH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
enum OAuthBodyError {
    #[error("OAuth HTTP response body exceeds {0} bytes")]
    TooLarge(usize),
}

/// The OAuth HTTP client for one server. See the module docs.
pub struct BoundedOAuthHttpClient {
    follow_redirects: reqwest::Client,
    stop_redirects: reqwest::Client,
}

impl BoundedOAuthHttpClient {
    /// A client that never uses a proxy from the environment, and uses `proxy`
    /// (the server's configured proxy URL) when one is given.
    pub fn new(proxy: Option<&str>) -> Result<Self, reqwest::Error> {
        let builder =
            |redirect: reqwest::redirect::Policy| -> Result<reqwest::Client, reqwest::Error> {
                let mut builder = reqwest::Client::builder()
                    .no_proxy()
                    .timeout(OAUTH_TIMEOUT)
                    .connect_timeout(OAUTH_TIMEOUT)
                    .redirect(redirect);
                if let Some(proxy) = proxy {
                    builder = builder.proxy(reqwest::Proxy::all(proxy)?);
                }
                builder.build()
            };
        let same_origin = reqwest::redirect::Policy::custom(|attempt| {
            let first = attempt.previous().first().cloned();
            if attempt.previous().len() > MAX_OAUTH_REDIRECTS {
                attempt.error("OAuth request exceeded its redirect limit")
            } else if first.is_some_and(|first| first.origin() == attempt.url().origin()) {
                attempt.follow()
            } else {
                attempt.error("OAuth redirect to another origin refused")
            }
        });
        Ok(Self {
            follow_redirects: builder(same_origin)?,
            stop_redirects: builder(reqwest::redirect::Policy::none())?,
        })
    }
}

impl OAuthHttpClient for BoundedOAuthHttpClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let boxed = |e: reqwest::Error| Box::new(e.without_url()) as OAuthHttpClientError;
            let OAuthHttpRequest {
                request,
                redirect_policy,
                timeout,
                ..
            } = request;
            let client = match redirect_policy {
                OAuthHttpRedirectPolicy::Stop => &self.stop_redirects,
                // `Follow`, and any policy a later `rmcp` adds, is followed only
                // within the same origin.
                _ => &self.follow_redirects,
            };
            let mut request = reqwest::Request::try_from(request).map_err(boxed)?;
            if let Some(timeout) = timeout {
                *request.timeout_mut() = Some(timeout.min(OAUTH_TIMEOUT));
            }
            let response = client.execute(request).await.map_err(boxed)?;
            let mut builder = http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(boxed)?;
                if chunk.len() > MAX_OAUTH_BODY_BYTES - body.len() {
                    return Err(Box::new(OAuthBodyError::TooLarge(MAX_OAUTH_BODY_BYTES))
                        as OAuthHttpClientError);
                }
                body.extend_from_slice(&chunk);
            }
            builder
                .body(body)
                .map_err(|e| Box::new(e) as OAuthHttpClientError)
        })
    }
}
