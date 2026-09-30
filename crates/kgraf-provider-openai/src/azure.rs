//! Azure OpenAI as a transport option of the two OpenAI adapters.
//!
//! Azure serves the Chat Completions and Responses bodies unchanged. What differs
//! is the address, the credential header, the meaning of the model name and the
//! content filter, so the existing adapters serve it through this option instead
//! of a second parser.
//!
//! **Address.** Two surfaces exist (Microsoft Learn, "Azure OpenAI in Microsoft
//! Foundry Models REST API reference" and "Azure OpenAI v1 API"):
//!
//! - dated, Chat Completions only:
//!   `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}`;
//! - v1: `https://{resource}.openai.azure.com/openai/v1/chat/completions` and
//!   `.../openai/v1/responses`, with the deployment as `model` in the body and no
//!   `api-version`.
//!
//! The embeddings body (decision 0092) is served the same way, at
//! `.../openai/v1/embeddings` with the deployment as `model`, or dated at
//! `.../openai/deployments/{deployment}/embeddings?api-version={version}` with the
//! deployment in the path (Microsoft Learn, "How to generate embeddings with Azure
//! OpenAI", <https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/embeddings>;
//! the 2024-10-21 inference specification, whose reply schema requires `model` and
//! `usage`). The version floor below does not apply to it.
//!
//! The configured API version picks the surface: `v1` is the v1 surface, a dated
//! value is the dated surface. Microsoft documents the Responses body only at
//! `/openai/v1/responses` (Microsoft Learn, "Azure OpenAI API version lifecycle"
//! and "Azure OpenAI v1 API"), so the Responses transport refuses a dated version
//! when it is built instead of posting to a path no page shows.
//!
//! **Version floor.** The lifecycle page lists `stream_options.include_usage`,
//! `completion_tokens_details.reasoning_tokens` and `max_completion_tokens` as added
//! in `2024-09-01-preview`. This transport streams with usage, reads reasoning
//! tokens and sends `max_completion_tokens`, so an older dated version is refused
//! when the transport is built: it would fail on the wire, or return no usage.
//!
//! **Credential.** `api-key: <key>`. A Microsoft Entra ID bearer token is not
//! supported: Microsoft's Rust crate (`azure_identity`) pulls in the whole Azure
//! SDK core, which is not a small dependency for one header.
//!
//! **Model name.** The deployment name stands in for the model in the request. The
//! response `model` names the underlying model instead, so the configuration must
//! map each deployment to the model it serves, and the echo check compares the
//! response against that mapping. A deployment with no mapping is refused when the
//! adapter is built; a response naming any other model raises.
//!
//! **Content filter.** A completion cut by the filter (`finish_reason:
//! "content_filter"`), a `filtered: true` in `prompt_filter_results`, or a
//! Responses `content_filters` entry with `blocked: true` settles the call, since
//! it billed, and raises `TurnNotUsable`. A prompt blocked outright is an HTTP 400
//! with code `content_filter`, which billed nothing and surfaces as the HTTP error.
//!
//! Not supported: sovereign-cloud hosts (`openai.azure.us`, `openai.azure.cn`) and
//! Foundry project hosts (`*.services.ai.azure.com`), because the host suffix is
//! fixed so that a configured resource name cannot steer a request to another host.
//!
//! **Deployments that cannot be mapped.** The response names the underlying model,
//! and this transport checks it against exactly one configured string per
//! deployment. A deployment that Azure updates automatically to a newer model
//! version, and a model-router deployment, answer with a model name that changes
//! or that differs per request, so no fixed string is right. They are not usable
//! here; deploy a fixed model version and map it.

use std::collections::BTreeMap;

use kgraf_provider::ProviderError;

/// Environment variable holding the API key.
pub const API_KEY_VARIABLE: &str = "KGRAF_AZURE_OPENAI_API_KEY";
/// Environment variable holding the resource name, the `{resource}` host label.
pub const RESOURCE_VARIABLE: &str = "KGRAF_AZURE_OPENAI_RESOURCE";
/// Environment variable holding the API version: `v1` or a dated version.
pub const API_VERSION_VARIABLE: &str = "KGRAF_AZURE_OPENAI_API_VERSION";
/// Environment variable mapping each deployment to the model it serves:
/// `deployment=model,deployment=model`.
pub const DEPLOYMENTS_VARIABLE: &str = "KGRAF_AZURE_OPENAI_DEPLOYMENTS";

/// The oldest dated version whose Chat Completions surface takes
/// `stream_options.include_usage` and `max_completion_tokens` (Microsoft Learn,
/// "Azure OpenAI API version lifecycle", changes between 2024-09-01-preview and
/// 2024-08-01-preview).
pub const MIN_DATED_CHAT_VERSION: &str = "2024-09-01";

/// The first dated version without `-preview` at or above the floor: the
/// documented GA version (Microsoft Learn, "Azure OpenAI REST API reference").
/// A date without the suffix between the floor and this one names no published
/// version. Beyond these two bounds the check cannot tell a published version
/// from an unpublished one: it is a floor, not a list.
pub const MIN_GA_CHAT_VERSION: &str = "2024-10-21";

/// The adapter name the Chat Completions option reports.
pub const CHAT_ADAPTER: &str = "azure-openai-chat";
/// The adapter name the Responses option reports.
pub const RESPONSES_ADAPTER: &str = "azure-openai-responses";
/// The adapter name the embeddings option reports.
pub const EMBEDDINGS_ADAPTER: &str = "azure-openai-embeddings";

/// A setting that is absent or unusable. Never carries the key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AzureSettingsError {
    /// A required variable is not set.
    #[error("{variable} is not set")]
    Missing { variable: &'static str },
    /// A variable is set to a value that cannot be used.
    #[error("{variable} is not usable: {reason}")]
    Invalid {
        variable: &'static str,
        reason: String,
    },
}

impl AzureSettingsError {
    /// The variable to set or change.
    pub fn variable(&self) -> &'static str {
        match self {
            AzureSettingsError::Missing { variable }
            | AzureSettingsError::Invalid { variable, .. } => variable,
        }
    }
}

fn invalid(variable: &'static str, reason: impl Into<String>) -> AzureSettingsError {
    AzureSettingsError::Invalid {
        variable,
        reason: reason.into(),
    }
}

/// Which address family the configured API version selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiVersion {
    /// The v1 surface: `/openai/v1/...`, no `api-version` query.
    V1,
    /// A dated surface, `YYYY-MM-DD` with an optional `-preview` suffix.
    Dated(String),
}

impl ApiVersion {
    /// Parse and validate. The value reaches a URL query, so only the two
    /// documented shapes are accepted.
    pub fn parse(value: &str) -> Result<Self, AzureSettingsError> {
        let value = value.trim();
        if value == "v1" {
            return Ok(ApiVersion::V1);
        }
        let date = value.strip_suffix("-preview").unwrap_or(value);
        let bytes = date.as_bytes();
        let shape = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !shape {
            return Err(invalid(
                API_VERSION_VARIABLE,
                "expected `v1` or a dated version such as 2024-10-21 or 2025-04-01-preview",
            ));
        }
        let month: u32 = date[5..7].parse().unwrap_or(0);
        let day: u32 = date[8..10].parse().unwrap_or(0);
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(invalid(
                API_VERSION_VARIABLE,
                "the date in the version is not a calendar date",
            ));
        }
        Ok(ApiVersion::Dated(value.to_owned()))
    }

    /// Refuse a version the transport cannot use, naming why.
    fn check_for(&self, kind: Kind) -> Result<(), AzureSettingsError> {
        match (kind, self) {
            (_, ApiVersion::V1) => Ok(()),
            // The embeddings body has been the same at every dated version the
            // lifecycle page lists, and nothing this transport sends or reads
            // there (`input`, `data`, `usage`) was added later: no floor.
            (Kind::Embeddings, ApiVersion::Dated(_)) => Ok(()),
            (Kind::Responses, ApiVersion::Dated(version)) => Err(invalid(
                API_VERSION_VARIABLE,
                format!(
                    "`{version}` cannot be used with the Responses route: Microsoft documents \
                     the Responses body only at /openai/v1/responses, so set the version to `v1`"
                ),
            )),
            (Kind::Chat, ApiVersion::Dated(version)) => {
                let preview = version.ends_with("-preview");
                let date = version.strip_suffix("-preview").unwrap_or(version);
                if !preview && (MIN_DATED_CHAT_VERSION..MIN_GA_CHAT_VERSION).contains(&date) {
                    Err(invalid(
                        API_VERSION_VARIABLE,
                        format!(
                            "`{version}` is not a published version: the first version without \
                             `-preview` at or above {MIN_DATED_CHAT_VERSION} is {MIN_GA_CHAT_VERSION}; \
                             add `-preview` or use {MIN_GA_CHAT_VERSION}, a newer version or `v1`"
                        ),
                    ))
                } else if date < MIN_DATED_CHAT_VERSION {
                    Err(invalid(
                        API_VERSION_VARIABLE,
                        format!(
                            "`{version}` is older than {MIN_DATED_CHAT_VERSION}-preview, the first \
                             version with stream usage and max_completion_tokens (Azure OpenAI \
                             API version lifecycle); use `v1` or a newer dated version"
                        ),
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    fn as_str(&self) -> &str {
        match self {
            ApiVersion::V1 => "v1",
            ApiVersion::Dated(version) => version,
        }
    }
}

/// Validated Azure OpenAI settings. `Debug` never prints the key.
#[derive(Clone, PartialEq, Eq)]
pub struct AzureSettings {
    api_key: String,
    resource: String,
    version: ApiVersion,
    deployments: BTreeMap<String, String>,
}

impl std::fmt::Debug for AzureSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureSettings")
            .field("resource", &self.resource)
            .field("version", &self.version)
            .field("deployments", &self.deployments)
            .finish_non_exhaustive()
    }
}

impl AzureSettings {
    /// Validate the four settings.
    ///
    /// `deployments` is `deployment=model,deployment=model`.
    pub fn new(
        api_key: &str,
        resource: &str,
        version: &str,
        deployments: &str,
    ) -> Result<Self, AzureSettingsError> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(AzureSettingsError::Missing {
                variable: API_KEY_VARIABLE,
            });
        }
        // The key becomes a header value and travels through a newline-framed
        // credential string, so whitespace and control characters are refused.
        if api_key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid(
                API_KEY_VARIABLE,
                "the key contains whitespace or a control character",
            ));
        }
        Ok(Self {
            api_key: api_key.to_owned(),
            resource: validate_resource(resource)?,
            version: ApiVersion::parse(version)?,
            deployments: parse_deployments(deployments)?,
        })
    }

    /// Read the settings through `lookup`, `std::env::var` in production.
    pub fn from_lookup(
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, AzureSettingsError> {
        let get = |variable: &'static str| {
            lookup(variable)
                .filter(|v| !v.trim().is_empty())
                .ok_or(AzureSettingsError::Missing { variable })
        };
        Self::new(
            &get(API_KEY_VARIABLE)?,
            &get(RESOURCE_VARIABLE)?,
            &get(API_VERSION_VARIABLE)?,
            &get(DEPLOYMENTS_VARIABLE)?,
        )
    }

    /// The four settings as one string, for the fleet's single-credential
    /// interface. Round-trips through [`Self::from_credential`].
    pub fn to_credential(&self) -> String {
        let deployments = self
            .deployments
            .iter()
            .map(|(deployment, model)| format!("{deployment}={model}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{}\n{}\n{}\n{}",
            self.api_key,
            self.resource,
            self.version.as_str(),
            deployments
        )
    }

    /// Parse a string made by [`Self::to_credential`], revalidating every part.
    pub fn from_credential(credential: &str) -> Result<Self, AzureSettingsError> {
        let mut parts = credential.splitn(4, '\n');
        let (Some(key), Some(resource), Some(version), Some(deployments)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(AzureSettingsError::Missing {
                variable: RESOURCE_VARIABLE,
            });
        };
        Self::new(key, resource, version, deployments)
    }

    /// Refuse a version the chosen surface cannot use. `responses` is true for the
    /// Responses route.
    pub fn check_for(&self, responses: bool) -> Result<(), AzureSettingsError> {
        self.version.check_for(if responses {
            Kind::Responses
        } else {
            Kind::Chat
        })
    }

    /// The API key, for registering with the secret scrubber.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The key a deployment's rate limits and 429s are recorded under. The adapters
    /// record under the same key.
    pub fn limit_key(&self, deployment: &str) -> String {
        kgraf_provider::limits::scoped_key(&self.origin(), deployment)
    }

    /// The deployment to underlying-model mapping.
    pub fn deployments(&self) -> &BTreeMap<String, String> {
        &self.deployments
    }

    /// `https://{resource}.openai.azure.com`.
    pub fn origin(&self) -> String {
        format!("https://{}.openai.azure.com", self.resource)
    }

    /// The configuration the adapters read: the key, and the deployments as the
    /// models the adapter may be asked for.
    pub fn provider_config(
        &self,
        adapter: &'static str,
        models: Vec<String>,
    ) -> Result<kgraf_provider::ProviderConfig, ProviderError> {
        kgraf_provider::ProviderConfig::new(
            adapter,
            self.origin(),
            self.api_key.clone(),
            models,
            kgraf_core::Protocol::Inclusive,
        )
    }
}

/// A resource name is a DNS label: 2 to 64 letters, digits and hyphens, with no
/// hyphen at either end. It reaches a hostname, so nothing else is accepted.
fn validate_resource(resource: &str) -> Result<String, AzureSettingsError> {
    let resource = resource.trim();
    let lowered = resource.to_ascii_lowercase();
    if lowered.contains("services.ai.azure") || lowered.contains("cognitiveservices.azure") {
        return Err(invalid(
            RESOURCE_VARIABLE,
            "Foundry project hosts (*.services.ai.azure.com) and cognitiveservices hosts are \
             not supported; give the name of an Azure OpenAI resource \
             (<name>.openai.azure.com) alone",
        ));
    }
    let ok = (2..=64).contains(&resource.len())
        && resource
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !resource.starts_with('-')
        && !resource.ends_with('-');
    if !ok {
        return Err(invalid(
            RESOURCE_VARIABLE,
            "expected the resource name alone (2 to 64 letters, digits or hyphens, none at \
             either end), not a URL or a host",
        ));
    }
    Ok(resource.to_ascii_lowercase())
}

/// Parse `deployment=model,deployment=model`. Deployment names reach a URL path,
/// so they are limited to the characters Azure allows in one.
fn parse_deployments(value: &str) -> Result<BTreeMap<String, String>, AzureSettingsError> {
    let mut map = BTreeMap::new();
    for pair in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((deployment, model)) = pair.split_once('=') else {
            return Err(invalid(
                DEPLOYMENTS_VARIABLE,
                "expected `deployment=model` pairs separated by commas",
            ));
        };
        let (deployment, model) = (deployment.trim(), model.trim());
        // A name of dots alone (`.` or `..`) is a path segment that steps up or
        // stays in place, so it never names a deployment.
        let deployment_ok = (1..=64).contains(&deployment.len())
            && !deployment.bytes().all(|b| b == b'.')
            && deployment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !deployment_ok {
            return Err(invalid(
                DEPLOYMENTS_VARIABLE,
                "a deployment name is 1 to 64 letters, digits, `-`, `_` or `.`",
            ));
        }
        let model_ok = (1..=128).contains(&model.len())
            && !model
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == ',');
        if !model_ok {
            return Err(invalid(
                DEPLOYMENTS_VARIABLE,
                "the model a deployment serves must be one non-empty token",
            ));
        }
        if map
            .insert(deployment.to_owned(), model.to_owned())
            .is_some()
        {
            return Err(invalid(
                DEPLOYMENTS_VARIABLE,
                "a deployment is listed twice",
            ));
        }
    }
    if map.is_empty() {
        return Err(AzureSettingsError::Missing {
            variable: DEPLOYMENTS_VARIABLE,
        });
    }
    Ok(map)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Chat,
    Responses,
    Embeddings,
}

impl Kind {
    fn adapter(self) -> &'static str {
        match self {
            Kind::Chat => CHAT_ADAPTER,
            Kind::Responses => RESPONSES_ADAPTER,
            Kind::Embeddings => EMBEDDINGS_ADAPTER,
        }
    }
}

/// The transport option an adapter is given to reach Azure OpenAI.
#[derive(Clone, Debug)]
pub struct AzureTransport {
    kind: Kind,
    origin: String,
    version: ApiVersion,
    deployments: BTreeMap<String, String>,
}

impl AzureTransport {
    /// Chat Completions transport. Refuses a dated version older than the first one
    /// with stream usage and `max_completion_tokens`.
    pub fn chat(settings: &AzureSettings) -> Result<Self, ProviderError> {
        Self::new(Kind::Chat, settings)
    }

    /// Responses transport. Refuses a dated version: only the v1 path is documented.
    pub fn responses(settings: &AzureSettings) -> Result<Self, ProviderError> {
        Self::new(Kind::Responses, settings)
    }

    /// Embeddings transport (decision 0092): `/openai/v1/embeddings` with the
    /// deployment as `model`, or on a dated version
    /// `/openai/deployments/{deployment}/embeddings?api-version=` with the
    /// deployment in the path. Any dated version is accepted: the embeddings
    /// body predates every version the chat floor exists for.
    pub fn embeddings(settings: &AzureSettings) -> Result<Self, ProviderError> {
        Self::new(Kind::Embeddings, settings)
    }

    fn new(kind: Kind, settings: &AzureSettings) -> Result<Self, ProviderError> {
        let adapter = kind.adapter();
        settings
            .version
            .check_for(kind)
            .map_err(|e| ProviderError::Unsent {
                adapter,
                reason: e.to_string(),
            })?;
        Ok(Self {
            kind,
            origin: settings.origin(),
            version: settings.version.clone(),
            deployments: settings.deployments.clone(),
        })
    }

    /// The key this deployment's rate limits and 429s are recorded under. Azure
    /// limits are per deployment. The host builds the same key from the settings,
    /// through [`AzureSettings::limit_key`].
    pub(crate) fn limit_key(&self, deployment: &str) -> String {
        kgraf_provider::limits::scoped_key(&self.origin, deployment)
    }

    /// Replace the origin. Used by tests that point the transport at a local server.
    #[cfg(test)]
    pub(crate) fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = origin.into().trim_end_matches('/').to_owned();
        self
    }

    /// The adapter name this transport reports to the ledger.
    pub fn name(&self) -> &'static str {
        self.kind.adapter()
    }

    /// The request URL for `deployment`, without the query.
    pub(crate) fn url(&self, deployment: &str) -> String {
        let path = match (self.kind, &self.version) {
            (Kind::Chat, ApiVersion::V1) => "/openai/v1/chat/completions".to_owned(),
            (Kind::Chat, ApiVersion::Dated(_)) => {
                format!("/openai/deployments/{deployment}/chat/completions")
            }
            (Kind::Embeddings, ApiVersion::V1) => "/openai/v1/embeddings".to_owned(),
            (Kind::Embeddings, ApiVersion::Dated(_)) => {
                format!("/openai/deployments/{deployment}/embeddings")
            }
            (Kind::Responses, ApiVersion::V1) => "/openai/v1/responses".to_owned(),
            // A dated Responses transport cannot be built (`check_for`), so this
            // is the v1 path whichever version is held.
            (Kind::Responses, _) => "/openai/v1/responses".to_owned(),
        };
        format!("{}{path}", self.origin)
    }

    /// The `api-version` query pair, absent on the v1 surface.
    pub(crate) fn query(&self) -> Option<(&'static str, &str)> {
        match &self.version {
            ApiVersion::V1 => None,
            ApiVersion::Dated(version) => Some(("api-version", version)),
        }
    }

    /// Whether the body names the model. The dated Chat Completions and
    /// embeddings surfaces take the deployment from the URL instead.
    pub(crate) fn body_names_model(&self) -> bool {
        self.kind == Kind::Responses || matches!(self.version, ApiVersion::V1)
    }

    /// The model `deployment` is configured to serve, which the response must name.
    pub(crate) fn underlying(
        &self,
        adapter: &'static str,
        deployment: &str,
    ) -> Result<&str, ProviderError> {
        self.deployments
            .get(deployment)
            .map(String::as_str)
            .ok_or_else(|| self.unmapped(adapter, deployment))
    }

    fn unmapped(&self, adapter: &'static str, deployment: &str) -> ProviderError {
        ProviderError::ModelUnsupported {
            adapter,
            model: deployment.to_owned(),
            available: self
                .deployments
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// Refuse a configuration that lists a model with no deployment mapping, so a
    /// response can never be accepted without a model to check it against.
    pub(crate) fn check_models(
        &self,
        adapter: &'static str,
        models: &[String],
    ) -> Result<(), ProviderError> {
        match models.iter().find(|m| !self.deployments.contains_key(*m)) {
            Some(model) => Err(self.unmapped(adapter, model)),
            None => Ok(()),
        }
    }
}

/// The `api-key` header value, marked sensitive so it stays out of debug output.
pub(crate) fn key_header(
    adapter: &'static str,
    key: &str,
) -> Result<reqwest::header::HeaderValue, ProviderError> {
    let mut value =
        reqwest::header::HeaderValue::from_str(key).map_err(|_| ProviderError::Unsent {
            adapter,
            reason: "the API key is not a valid header value".to_owned(),
        })?;
    value.set_sensitive(true);
    Ok(value)
}

/// True when a filter tree carries an `error`: the filters did not run, so the
/// content was never checked (Microsoft Learn, "Content filtering", which says to
/// verify there is no error object in `content_filter_results`). Never true for a category entry, which has
/// `filtered`, `severity` and `detected` only.
fn any_filter_error(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, v)| (key == "error" && !v.is_null()) || any_filter_error(v)),
        serde_json::Value::Array(items) => items.iter().any(any_filter_error),
        _ => false,
    }
}

/// True when a `content_filter_results` tree reports any category as filtered.
fn any_filtered(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, v)| (key == "filtered" && v.as_bool() == Some(true)) || any_filtered(v)),
        serde_json::Value::Array(items) => items.iter().any(any_filtered),
        _ => false,
    }
}

/// Whether a Chat Completions body, or one streamed chunk, says the filter acted.
///
/// Returns the stop reason to raise with: `content_filter` for a cut completion,
/// `prompt_filter_results` for a filtered prompt.
pub(crate) fn chat_filter(body: &serde_json::Value) -> Option<&'static str> {
    if body.get("prompt_filter_results").is_some_and(any_filtered) {
        return Some("prompt_filter_results");
    }
    let choices = body.get("choices").and_then(|c| c.as_array());
    let cut = choices.is_some_and(|choices| {
        choices.iter().any(|choice| {
            choice.get("finish_reason").and_then(|f| f.as_str()) == Some("content_filter")
                || choice
                    .get("content_filter_results")
                    .is_some_and(any_filtered)
        })
    });
    if cut {
        return Some("content_filter");
    }
    // The filters did not run on this content: it went out unchecked, which a
    // deployment with filtering configured must not do silently.
    let unchecked = body
        .get("prompt_filter_results")
        .is_some_and(any_filter_error)
        || choices.is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .get("content_filter_results")
                    .is_some_and(any_filter_error)
            })
        });
    unchecked.then_some("content_filter_results_error")
}

/// Whether a Responses object says the filter acted, and the stop reason to raise
/// with.
///
/// Microsoft Learn ("Responses API", the guardrails section) describes a top-level
/// `content_filters` array, one entry per filter result, each with `blocked`,
/// `source_type` and `content_filter_results` (categories with `filtered`). An
/// entry with `blocked: true` cut the turn; an entry with no `blocked` field and a
/// category `filtered: true` did too, and an explicit `blocked: false` is
/// respected. An incomplete response whose reason is the filter is a cut. Failing
/// that, an `error` inside an entry's `content_filter_results` means the filters
/// did not run (`content_filter_results_error`), as for Chat Completions.
pub(crate) fn responses_filter(body: &serde_json::Value) -> Option<&'static str> {
    let entries = body.get("content_filters").and_then(|f| f.as_array());
    let blocked = entries.is_some_and(|entries| {
        entries.iter().any(
            |entry| match entry.get("blocked").and_then(|b| b.as_bool()) {
                Some(blocked) => blocked,
                None => entry
                    .get("content_filter_results")
                    .is_some_and(any_filtered),
            },
        )
    });
    let incomplete = body
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str())
        == Some("content_filter");
    if blocked || incomplete {
        return Some("content_filter");
    }
    let unchecked = entries.is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry
                .get("content_filter_results")
                .is_some_and(any_filter_error)
        })
    });
    unchecked.then_some("content_filter_results_error")
}
