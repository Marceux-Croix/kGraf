//! The MCP client (decision 0051), built on the `rmcp` crate.
//!
//! This file set is the Wave 0 seam, with no `rmcp` dependency yet:
//!
//! - [`McpServerConfig`]: per server, the transport (stdio or Streamable HTTP), the
//!   command or URL, headers, a timeout and the server's instructions. A value of
//!   this type is always valid; secrets are redacted on output. The shape follows
//!   LibreChat's MCP config (MIT).
//! - [`McpCaller`]: the one call the MCP stream implements over `rmcp`.
//! - [`replace_server_tools`]: how discovered tools enter a `ToolRegistry`, under
//!   `mcp__<server>__<tool>` names, with each refused tool reported.
//!
//! Since the Wave 0 seam above was written, this crate has grown the concrete
//! implementation:
//!
//! - [`client::RmcpCaller`]: [`McpCaller`] on the official `rmcp` crate, for both
//!   stdio and Streamable HTTP servers.
//! - [`oauth`]: OAuth for hosted Streamable HTTP servers, on `rmcp`'s own
//!   `transport::auth`, with tokens held in the OS keychain.
//! - [`parse`]: `tools/call` result parsing (binary summarized, metadata
//!   flattened, sizes capped), ported from LibreChat's `parsers.ts`.
//! - `http`: the Streamable HTTP policy on top of `rmcp`'s transport: header
//!   values read from the environment ([`HeaderFromEnv`]), credentials scrubbed
//!   from all server output, typed errors for HTTP status, session expiry and
//!   protocol version ([`McpConnectError`]).

pub mod attribution;
mod client;
mod config;
mod http;
mod http_client;
pub mod oauth;
mod oauth_http;
mod parse;
mod register;

pub use attribution::{
    AttributionLost, InMemoryAttributionSink, LedgerAttributionSink, McpAttributionSink,
    McpCallEvent, McpCallKind, McpCallOutcome, TracingAttributionSink,
};
pub use client::{INHERITED_ENV_ALLOWLIST, McpConnectError, RmcpCaller, base_http_client_builder};
pub use config::{
    DEFAULT_TIMEOUT_MS, HeaderFromEnv, McpConfigError, McpServerConfig, McpTransport, REDACTED,
    RESERVED_HEADERS, validate_all,
};
pub use http::{MAX_ERROR_DETAIL_CHARS, MIN_SECRET_CHARS, SUPPORTED_HTTP_PROTOCOL_VERSIONS};
pub use http_client::{
    GET_STREAM_BUDGET_WINDOW, MAX_ERROR_BODY_BYTES, MAX_GET_STREAM_REOPENS, MAX_HTTP_BODY_BYTES,
    MIN_GET_STREAM_INTERVAL,
};
pub use parse::{MAX_METADATA_CHARS, MAX_TEXT_PART_CHARS, parse_call_tool_result};
pub use register::{
    DESCRIPTION_ENVELOPE_TAG, DiscoveredTool, MAX_DESCRIPTION_CHARS, McpCaller,
    McpRegistrationError, McpTool, RegistrationReport, replace_server_tools,
};
