//! The OAuth HTTP client (`BoundedOAuthHttpClient`) against a real, local token
//! endpoint, driven by `rmcp`'s own `AuthorizationManager` refresh.
//! The proxy case is in `http_proxy_env.rs`, which may set process environment.

mod common;

use common::{refresh_through, token_endpoint};
use kgraf_mcp::oauth::BoundedOAuthHttpClient;

#[tokio::test]
async fn a_token_refresh_goes_to_the_token_endpoint() {
    let endpoint = token_endpoint(None).await;
    let client = BoundedOAuthHttpClient::new(None).expect("client");
    refresh_through(client, &endpoint.url)
        .await
        .expect("the refresh succeeds");
    let seen = endpoint.requests.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains("refresh-SECRET-1"),
        "the refresh token was sent"
    );
}

/// A 30x on refresh is an error, and the redirect target never receives the
/// refresh token.
#[tokio::test]
async fn a_redirect_on_refresh_is_an_error_and_is_not_followed() {
    let target = token_endpoint(None).await;
    let endpoint = token_endpoint(Some(target.url.clone())).await;
    let client = BoundedOAuthHttpClient::new(None).expect("client");
    let result = refresh_through(client, &endpoint.url).await;
    assert!(result.is_err(), "a 307 on refresh must fail the refresh");
    assert_eq!(endpoint.requests.lock().expect("lock").len(), 1);
    assert!(
        target.requests.lock().expect("lock").is_empty(),
        "the redirect was followed"
    );
}
