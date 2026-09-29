//! `HTTP_PROXY` in the environment must not reroute MCP or OAuth egress. A proxy
//! would receive the request line and every header, including `Authorization`,
//! and plain HTTP to loopback would reach it in clear.
//!
//! Its own test binary, with one test: it sets process environment variables,
//! which is only sound while no other thread reads the environment.

mod common;

use common::{Options, Reply, config, fake, refresh_through, token_endpoint};
use kgraf_mcp::McpCallKind;
use kgraf_mcp::oauth::{BoundedOAuthHttpClient, OAuthConnector};
use kgraf_mcp::{HeaderFromEnv, InMemoryAttributionSink, RmcpCaller};
use std::sync::Arc;

#[tokio::test]
async fn an_http_proxy_variable_does_not_reroute_mcp_or_oauth_traffic() {
    let proxy = fake(Options::reply(Reply::Json("hi"))).await;
    let proxy_address = proxy.url.trim_end_matches("/mcp").to_owned();
    // SAFETY: the only test in this binary; set before any client is built and
    // before any other task runs.
    unsafe {
        std::env::set_var("HTTP_PROXY", &proxy_address);
        std::env::set_var("http_proxy", &proxy_address);
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }
    // Nothing listens on port 9: without a proxy the connect fails.
    let config = config(
        "http://127.0.0.1:9/mcp",
        &[(
            "Authorization",
            HeaderFromEnv::new("T").with_prefix("Bearer "),
        )],
    );
    let result = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &|_| Ok("PROXY-SEEN-SECRET".to_owned()),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await;
    let seen = proxy.requests.lock().expect("lock").clone();
    assert!(seen.is_empty(), "the proxy received: {seen:?}");
    assert!(result.is_err(), "no server listens on the configured port");

    // OAuth token refresh, which carries the refresh token, goes to the
    // token endpoint and never to the environment's proxy.
    let endpoint = token_endpoint(None).await;
    refresh_through(
        BoundedOAuthHttpClient::new(None).expect("client"),
        &endpoint.url,
    )
    .await
    .expect("the refresh reaches the token endpoint directly");
    assert_eq!(endpoint.requests.lock().expect("lock").len(), 1);
    let seen = proxy.requests.lock().expect("lock").clone();
    assert!(seen.is_empty(), "the proxy received: {seen:?}");

    // the production path. `OAuthConnector::new` builds the manager with the
    // proxy-free client, and one discovery request from `begin` goes to the
    // server, not the proxy. `begin` then fails (the fake is no OAuth server);
    // only where its request went matters here.
    let server = fake(Options::reply(Reply::Json("unused"))).await;
    let sink = Arc::new(InMemoryAttributionSink::new());
    let mut connector = OAuthConnector::new("remote", &server.url, None, sink.clone())
        .await
        .expect("connector");
    let _ = connector
        .begin("kgraf-test", "http://127.0.0.1:1/callback", &[])
        .await;
    assert!(
        !server.requests.lock().expect("lock").is_empty(),
        "discovery reached the server"
    );
    let seen = proxy.requests.lock().expect("lock").clone();
    assert!(seen.is_empty(), "the proxy received: {seen:?}");
    assert!(
        sink.events().iter().any(|e| e.kind == McpCallKind::OAuth),
        "the discovery attempt is attributed"
    );

    // Control: `rmcp`'s default OAuth client, which kGraf used before, does
    // send the refresh through the proxy. Without this, the check above could
    // pass because the proxy variable was simply not read.
    let default_client =
        rmcp::transport::auth::default_oauth_http_client().expect("default client");
    let _ = refresh_through(default_client, &endpoint.url).await;
    let seen = proxy.requests.lock().expect("lock").clone();
    assert!(
        seen.iter().any(|r| r.contains("refresh-SECRET-1")),
        "the default client should have used the proxy: {seen:?}"
    );
}
