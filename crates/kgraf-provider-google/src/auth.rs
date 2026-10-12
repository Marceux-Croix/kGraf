//! Google OAuth for Vertex AI, and the Vertex settings a route needs.
//!
//! Token handling is `gcp_auth` (MIT): service-account key files, the gcloud
//! user credentials file, the metadata server, the gcloud CLI, and token caching
//! and refresh. Reused rather than written here because JWT signing and token
//! refresh are security code; see the workspace `Cargo.toml` for what else was
//! compared.
//!
//! **Each source is explicit.** `gcp_auth::provider()` tries every source in
//! turn, including an HTTP probe of the metadata server and running the `gcloud`
//! binary. Neither is an egress the user configured, so this module builds the
//! provider for the one source that was chosen and nothing else:
//!
//! - `adc`: the service-account key file named by `GOOGLE_APPLICATION_CREDENTIALS`
//!   if that variable is set, otherwise the gcloud user credentials file
//!   (`gcloud auth application-default login`). File types outside those two,
//!   such as `external_account` (workload identity federation) files, are not
//!   supported and fail with the file's own error.
//! - a path: a service-account key file.
//! - `metadata`: the metadata server, on a Google Cloud host.
//! - `gcloud`: the `gcloud auth print-access-token` command.
//!
//! What this module adds is small:
//!
//! - [`VertexSettings`]: credentials source, project and region, read from the
//!   environment and validated before they reach a URL or a hostname.
//! - [`GoogleAuth`]: a [`BearerSource`] over `gcp_auth`. A service-account key
//!   file is read and parsed when the adapter is built, so a bad file fails at
//!   startup. The other sources are initialized on the first request, because
//!   initializing them reads the user's gcloud files, calls the metadata
//!   server, or runs `gcloud`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gcp_auth::TokenProvider;
use kgraf_provider::ProviderError;
use kgraf_provider_anthropic::BearerSource;
use tokio::sync::OnceCell;

/// The OAuth scope Vertex AI accepts.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The variable that selects the credentials source.
pub const CREDENTIALS_VARIABLE: &str = "KGRAF_VERTEX_CREDENTIALS";
/// The variable that selects the project.
pub const PROJECT_VARIABLE: &str = "KGRAF_VERTEX_PROJECT";
/// The variable that selects the region.
pub const REGION_VARIABLE: &str = "KGRAF_VERTEX_REGION";

/// Where Vertex credentials come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// Application Default Credentials from files only: the service-account
    /// key file `GOOGLE_APPLICATION_CREDENTIALS` names, else the gcloud user
    /// credentials file. Never the metadata server and never the gcloud CLI.
    ApplicationDefault,
    /// A service-account key file.
    ServiceAccountFile(PathBuf),
    /// The metadata server of the Google Cloud host kGraf runs on.
    MetadataServer,
    /// The `gcloud auth print-access-token` command.
    GcloudCli,
}

impl CredentialSource {
    /// `adc`, `metadata` and `gcloud` (any case) select those sources. Any other
    /// value is a path to a service-account key file; a file with one of those
    /// names is written `./adc`.
    pub fn parse(value: &str) -> Self {
        let value = value.trim();
        if value.eq_ignore_ascii_case("adc") {
            CredentialSource::ApplicationDefault
        } else if value.eq_ignore_ascii_case("metadata") {
            CredentialSource::MetadataServer
        } else if value.eq_ignore_ascii_case("gcloud") {
            CredentialSource::GcloudCli
        } else {
            CredentialSource::ServiceAccountFile(PathBuf::from(value))
        }
    }

    fn label(&self) -> &'static str {
        match self {
            CredentialSource::ApplicationDefault => "adc",
            CredentialSource::ServiceAccountFile(_) => "service-account file",
            CredentialSource::MetadataServer => "metadata",
            CredentialSource::GcloudCli => "gcloud",
        }
    }

    fn as_setting(&self) -> String {
        match self {
            CredentialSource::ApplicationDefault => "adc".to_owned(),
            CredentialSource::MetadataServer => "metadata".to_owned(),
            CredentialSource::GcloudCli => "gcloud".to_owned(),
            CredentialSource::ServiceAccountFile(path) => path.to_string_lossy().into_owned(),
        }
    }
}

/// A setting that is absent or unusable.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingError {
    /// No value in any of the variables read.
    #[error("{variable} is not set; {hint}")]
    Missing {
        /// The kGraf variable to set.
        variable: &'static str,
        /// What to set it to.
        hint: &'static str,
    },
    /// A value was found and cannot be used.
    #[error("{variable} cannot be used: {reason}")]
    Invalid {
        variable: &'static str,
        reason: String,
    },
}

impl SettingError {
    /// The variable the user should set.
    pub fn variable(&self) -> &'static str {
        match self {
            SettingError::Missing { variable, .. } | SettingError::Invalid { variable, .. } => {
                variable
            }
        }
    }
}

/// What a Vertex route needs beyond a model name.
///
/// Carried between the catalog's credential step and the adapter build as one
/// string ([`VertexSettings::to_credential`]), so a missing or malformed value
/// fails where the credential is resolved, before any request, and the
/// plumbing that passes one credential string per route needs no second
/// channel. None of the three values is a secret.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VertexSettings {
    credentials: String,
    project: String,
    region: String,
}

impl VertexSettings {
    /// Settings from explicit values, validated.
    pub fn new(
        credentials: CredentialSource,
        project: &str,
        region: &str,
    ) -> Result<Self, SettingError> {
        let project = project.trim();
        let region = region.trim();
        validate_project(project)?;
        validate_region(region)?;
        Ok(Self {
            credentials: credentials.as_setting(),
            project: project.to_owned(),
            region: region.to_owned(),
        })
    }

    /// Settings from the environment, through `lookup`.
    ///
    /// Credentials: `KGRAF_VERTEX_CREDENTIALS`, then
    /// `GOOGLE_APPLICATION_CREDENTIALS`. Project: `KGRAF_VERTEX_PROJECT`, then
    /// `GOOGLE_CLOUD_PROJECT`, then `GCLOUD_PROJECT`, then the `project_id` in a
    /// service-account key file. Region: `KGRAF_VERTEX_REGION`, then
    /// `GOOGLE_CLOUD_LOCATION`, then `GOOGLE_CLOUD_REGION`. There is no default
    /// region: the choice sets where the data is processed and which models
    /// exist.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, SettingError> {
        let first = |names: &[&str]| {
            names
                .iter()
                .filter_map(|name| lookup(name))
                .map(|value| value.trim().to_owned())
                .find(|value| !value.is_empty())
        };
        let credentials = first(&[CREDENTIALS_VARIABLE, "GOOGLE_APPLICATION_CREDENTIALS"])
            .map(|value| CredentialSource::parse(&value))
            .ok_or(SettingError::Missing {
                variable: CREDENTIALS_VARIABLE,
                hint: "set it to `adc` (a service-account key file named by \
                       GOOGLE_APPLICATION_CREDENTIALS, or the gcloud user credentials file), \
                       `metadata`, `gcloud`, or the path of a service-account key file",
            })?;
        let region = first(&[
            REGION_VARIABLE,
            "GOOGLE_CLOUD_LOCATION",
            "GOOGLE_CLOUD_REGION",
        ])
        .ok_or(SettingError::Missing {
            variable: REGION_VARIABLE,
            hint: "set it to a Vertex region such as `us-central1`, or `global`",
        })?;
        let project = first(&[PROJECT_VARIABLE, "GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"])
            .or_else(|| match &credentials {
                CredentialSource::ServiceAccountFile(path) => project_in_key_file(path),
                CredentialSource::ApplicationDefault
                | CredentialSource::MetadataServer
                | CredentialSource::GcloudCli => None,
            })
            .ok_or(SettingError::Missing {
                variable: PROJECT_VARIABLE,
                hint: "set it to the Google Cloud project id that is billed",
            })?;
        Self::new(credentials, &project, &region)
    }

    /// The single string the catalog hands to the build step.
    pub fn to_credential(&self) -> String {
        // Three plain strings always serialize.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The inverse of [`VertexSettings::to_credential`], validated again.
    pub fn from_credential(value: &str) -> Result<Self, SettingError> {
        let parsed: Self = serde_json::from_str(value).map_err(|_| SettingError::Invalid {
            variable: CREDENTIALS_VARIABLE,
            reason: "the Vertex settings were not produced by this build; resolve the \
                     route's credential through the catalog"
                .to_owned(),
        })?;
        validate_project(&parsed.project)?;
        validate_region(&parsed.region)?;
        Ok(parsed)
    }

    /// The credentials source.
    pub fn credentials(&self) -> CredentialSource {
        CredentialSource::parse(&self.credentials)
    }

    /// The project id.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// The region.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The API host for the region. `global` has no region prefix, and the
    /// multi-regions `us` and `eu` have their own hosts.
    /// <https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/use-partner-models>
    pub fn api_host(&self) -> String {
        match self.region.as_str() {
            "global" => "aiplatform.googleapis.com".to_owned(),
            multi @ ("us" | "eu") => format!("aiplatform.{multi}.rep.googleapis.com"),
            region => format!("{region}-aiplatform.googleapis.com"),
        }
    }

    /// `https://{host}/v1/projects/{project}/locations/{region}/publishers/{publisher}`,
    /// the root a model segment is appended to.
    pub fn publisher_root(&self, publisher: &str) -> String {
        format!(
            "https://{}/v1/projects/{}/locations/{}/publishers/{publisher}",
            self.api_host(),
            self.project,
            self.region
        )
    }
}

/// A region is a hostname label; anything outside lowercase letters, digits and
/// hyphens could redirect the request to another host.
fn validate_region(region: &str) -> Result<(), SettingError> {
    let ok = !region.is_empty()
        && region.len() <= 40
        && region.starts_with(|c: char| c.is_ascii_lowercase())
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(SettingError::Invalid {
            variable: REGION_VARIABLE,
            reason: "a region is lowercase letters, digits and hyphens, such as `us-central1`"
                .to_owned(),
        })
    }
}

/// A project id is a path segment; the character set allows the
/// domain-scoped form (`example.com:project`) and nothing that changes the path.
fn validate_project(project: &str) -> Result<(), SettingError> {
    let ok = !project.is_empty()
        && project.len() <= 128
        && project.starts_with(|c: char| c.is_ascii_alphanumeric())
        && project
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if ok {
        Ok(())
    } else {
        Err(SettingError::Invalid {
            variable: PROJECT_VARIABLE,
            reason: "a project id is letters, digits, hyphens, underscores, dots and colons"
                .to_owned(),
        })
    }
}

/// The `project_id` of a service-account key file, if the file has one. Any
/// failure reads as absent here; the build step reports an unreadable file.
fn project_in_key_file(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = value.get("project_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// The credential files a source reads, in the order they are tried.
///
/// Application Default Credentials read the key file `GOOGLE_APPLICATION_CREDENTIALS`
/// names, else the gcloud user credentials file; both are listed when they
/// exist, since either may be the one used.
fn credential_files(
    source: &CredentialSource,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    match source {
        CredentialSource::ServiceAccountFile(path) => vec![path.clone()],
        CredentialSource::ApplicationDefault => {
            let mut files = Vec::new();
            if let Some(path) = lookup("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
                files.push(PathBuf::from(path));
            }
            #[cfg(unix)]
            let user = std::env::home_dir()
                .map(|home| home.join(".config/gcloud/application_default_credentials.json"));
            #[cfg(not(unix))]
            let user = lookup("APPDATA")
                .map(|dir| PathBuf::from(dir).join("gcloud/application_default_credentials.json"));
            files.extend(user);
            files
        }
        CredentialSource::MetadataServer | CredentialSource::GcloudCli => Vec::new(),
    }
}

/// The values in the credential files of `source` that must never reach a
/// transcript: a service-account `private_key` and `private_key_id`, and a user
/// credential's `refresh_token` and `client_secret`.
///
/// A key is several lines, and a file spells its line breaks as `\n`, so the
/// value is returned in both spellings and as each of its body lines: a tool
/// that prints the file, or a few lines of it, is caught by one of them. A
/// missing or unparseable file yields nothing here; building the adapter
/// reports it.
///
/// The kind is a label for the scrubber's notices; no value is logged.
pub fn credential_file_secrets(
    source: &CredentialSource,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<(&'static str, String)> {
    const FIELDS: [(&str, &str); 4] = [
        ("private_key", "vertex-private-key"),
        ("private_key_id", "vertex-private-key-id"),
        ("refresh_token", "vertex-refresh-token"),
        ("client_secret", "vertex-client-secret"),
    ];
    let mut out = Vec::new();
    for path in credential_files(source, lookup) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        for (field, kind) in FIELDS {
            let Some(value) = json
                .get(field)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            out.push((kind, value.to_owned()));
            if value.contains('\n') {
                out.push((kind, value.replace('\n', "\\n")));
                out.extend(
                    value
                        .lines()
                        .map(str::trim)
                        .filter(|line| line.len() >= 16 && !line.starts_with("-----"))
                        .map(|line| (kind, line.to_owned())),
                );
            }
        }
    }
    out
}

/// The quota project of a user credential, for the `x-goog-user-project`
/// header. A user credential is billed to the project of the OAuth client that
/// minted it unless the request names another, and Vertex AI is usually not
/// enabled there; the credentials file records the project `gcloud` was told
/// to use. A service account has none: its own project is billed.
/// <https://cloud.google.com/docs/authentication/rest#set-billing-project>
fn quota_project(
    source: &CredentialSource,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if !matches!(source, CredentialSource::ApplicationDefault) {
        return None;
    }
    // The first file that exists is the one `gcp_auth` reads.
    let path = credential_files(source, lookup)
        .into_iter()
        .find(|p| p.is_file())?;
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if json.get("type").and_then(|t| t.as_str()) != Some("authorized_user") {
        return None;
    }
    let id = json.get("quota_project_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// A [`BearerSource`] over `gcp_auth`.
pub struct GoogleAuth {
    adapter: &'static str,
    source: CredentialSource,
    timeout: Duration,
    provider: OnceCell<Arc<dyn TokenProvider>>,
    quota_project: Option<String>,
}

impl GoogleAuth {
    /// Prepare authentication for `source`.
    ///
    /// A service-account key file is read and parsed now. Application Default
    /// Credentials are discovered on the first request.
    pub fn new(
        adapter: &'static str,
        source: CredentialSource,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let provider = OnceCell::new();
        if let CredentialSource::ServiceAccountFile(path) = &source {
            let account = gcp_auth::CustomServiceAccount::from_file(path).map_err(|e| {
                ProviderError::Decode {
                    adapter,
                    reason: format!(
                        "cannot use the service-account key file `{}`: {e}",
                        path.display()
                    ),
                }
            })?;
            let account: Arc<dyn TokenProvider> = Arc::new(account);
            // A fresh cell is empty, so this cannot fail.
            let _ = provider.set(account);
        }
        let quota_project = quota_project(&source, &|name| std::env::var(name).ok());
        Ok(Self {
            adapter,
            source,
            timeout,
            provider,
            quota_project,
        })
    }

    async fn provider(&self) -> Result<&Arc<dyn TokenProvider>, ProviderError> {
        self.provider
            .get_or_try_init(|| async {
                let mut failure = None;
                for step in steps(&self.source) {
                    match step.run().await {
                        Ok(Some(provider)) => return Ok(provider),
                        Ok(None) => {}
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                Err(ProviderError::Unsent {
                    adapter: self.adapter,
                    reason: format!(
                        "no Google credentials found for the `{}` source{}; {CREDENTIALS_VARIABLE} \
                         takes `adc`, `metadata`, `gcloud`, or a service-account key file path",
                        self.source.label(),
                        failure.map(|e| format!(" ({e})")).unwrap_or_default()
                    ),
                })
            })
            .await
    }

    /// The credentials source this instance was built with.
    pub fn source(&self) -> &CredentialSource {
        &self.source
    }
}

/// One way of obtaining a token provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The key file `GOOGLE_APPLICATION_CREDENTIALS` names, if the variable is set.
    EnvServiceAccount,
    /// The gcloud user credentials file.
    UserCredentialsFile,
    /// The metadata server.
    Metadata,
    /// The gcloud CLI.
    Gcloud,
}

/// The steps for a source, in order. Only `metadata` reaches the metadata
/// server and only `gcloud` runs the CLI.
fn steps(source: &CredentialSource) -> &'static [Step] {
    match source {
        CredentialSource::ApplicationDefault => {
            &[Step::EnvServiceAccount, Step::UserCredentialsFile]
        }
        CredentialSource::MetadataServer => &[Step::Metadata],
        CredentialSource::GcloudCli => &[Step::Gcloud],
        // Built eagerly in `GoogleAuth::new`.
        CredentialSource::ServiceAccountFile(_) => &[],
    }
}

impl Step {
    /// `Ok(None)` means this step does not apply and the next may be tried.
    async fn run(self) -> Result<Option<Arc<dyn TokenProvider>>, gcp_auth::Error> {
        Ok(match self {
            Step::EnvServiceAccount => gcp_auth::CustomServiceAccount::from_env()?
                .map(|account| Arc::new(account) as Arc<dyn TokenProvider>),
            Step::UserCredentialsFile => {
                Some(Arc::new(gcp_auth::ConfigDefaultCredentials::new().await?)
                    as Arc<dyn TokenProvider>)
            }
            Step::Metadata => {
                Some(Arc::new(gcp_auth::MetadataServiceAccount::new().await?)
                    as Arc<dyn TokenProvider>)
            }
            Step::Gcloud => {
                Some(Arc::new(gcp_auth::GCloudAuthorizedUser::new().await?)
                    as Arc<dyn TokenProvider>)
            }
        })
    }
}

#[async_trait::async_trait]
impl BearerSource for GoogleAuth {
    fn quota_project(&self) -> Option<String> {
        self.quota_project.clone()
    }

    async fn bearer(&self) -> Result<String, ProviderError> {
        let fetch = async {
            let provider = self.provider().await?;
            let token = provider.token(&[CLOUD_PLATFORM_SCOPE]).await.map_err(|e| {
                ProviderError::Unsent {
                    adapter: self.adapter,
                    reason: format!("could not obtain a Google access token: {e}"),
                }
            })?;
            Ok(token.as_str().to_owned())
        };
        match tokio::time::timeout(self.timeout, fetch).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Unsent {
                adapter: self.adapter,
                reason: format!(
                    "obtaining a Google access token took longer than {:?}",
                    self.timeout
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn settings_resolve_from_the_kgraf_variables() {
        let settings = VertexSettings::from_lookup(env(&[
            ("KGRAF_VERTEX_CREDENTIALS", "adc"),
            ("KGRAF_VERTEX_PROJECT", "my-project"),
            ("KGRAF_VERTEX_REGION", "us-east5"),
        ]))
        .expect("settings");
        assert_eq!(settings.credentials(), CredentialSource::ApplicationDefault);
        assert_eq!(settings.project(), "my-project");
        assert_eq!(settings.region(), "us-east5");
        assert_eq!(settings.api_host(), "us-east5-aiplatform.googleapis.com");
        assert_eq!(
            settings.publisher_root("google"),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/my-project/locations/us-east5/publishers/google"
        );
    }

    #[test]
    fn the_google_variables_are_the_fallback_and_the_kgraf_ones_win() {
        let settings = VertexSettings::from_lookup(env(&[
            ("GOOGLE_APPLICATION_CREDENTIALS", "/keys/sa.json"),
            ("GOOGLE_CLOUD_PROJECT", "from-google"),
            ("GOOGLE_CLOUD_LOCATION", "europe-west4"),
            ("KGRAF_VERTEX_PROJECT", "from-kgraf"),
        ]))
        .expect("settings");
        assert_eq!(settings.project(), "from-kgraf");
        assert_eq!(settings.region(), "europe-west4");
        assert_eq!(
            settings.credentials(),
            CredentialSource::ServiceAccountFile("/keys/sa.json".into())
        );
    }

    #[test]
    fn a_missing_setting_names_the_variable_to_set() {
        let none = |_: &str| None;
        let error = VertexSettings::from_lookup(none).expect_err("nothing set");
        assert_eq!(error.variable(), CREDENTIALS_VARIABLE);
        let error = VertexSettings::from_lookup(env(&[("KGRAF_VERTEX_CREDENTIALS", "adc")]))
            .expect_err("no region");
        assert_eq!(error.variable(), REGION_VARIABLE);
        let error = VertexSettings::from_lookup(env(&[
            ("KGRAF_VERTEX_CREDENTIALS", "adc"),
            ("KGRAF_VERTEX_REGION", "us-central1"),
        ]))
        .expect_err("no project");
        assert_eq!(error.variable(), PROJECT_VARIABLE);
    }

    #[test]
    fn the_project_falls_back_to_the_key_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("sa.json");
        std::fs::write(
            &path,
            r#"{"type":"service_account","project_id":"file-project"}"#,
        )
        .expect("write");
        let settings = VertexSettings::from_lookup(env(&[
            ("KGRAF_VERTEX_CREDENTIALS", path.to_str().expect("utf8")),
            ("KGRAF_VERTEX_REGION", "global"),
        ]))
        .expect("settings");
        assert_eq!(settings.project(), "file-project");
        assert_eq!(settings.api_host(), "aiplatform.googleapis.com");
    }

    #[test]
    fn a_region_or_project_that_could_change_the_host_or_path_is_refused() {
        for region in [
            "evil.example.com/x",
            "us-central1@host",
            "US-CENTRAL1",
            "1abc",
            "",
        ] {
            assert!(
                VertexSettings::new(CredentialSource::ApplicationDefault, "p", region).is_err(),
                "{region}"
            );
        }
        for project in ["a/b", "a b", "p?x=1", "../x", ""] {
            assert!(
                VertexSettings::new(CredentialSource::ApplicationDefault, project, "us-central1")
                    .is_err(),
                "{project}"
            );
        }
        assert!(
            VertexSettings::new(
                CredentialSource::ApplicationDefault,
                "example.com:my-project",
                "us-central1"
            )
            .is_ok()
        );
    }

    #[test]
    fn the_credential_string_round_trips_and_rejects_anything_else() {
        let settings = VertexSettings::new(
            CredentialSource::ServiceAccountFile("/keys/sa.json".into()),
            "p",
            "us-central1",
        )
        .expect("settings");
        let back = VertexSettings::from_credential(&settings.to_credential()).expect("parse");
        assert_eq!(back, settings);
        assert!(VertexSettings::from_credential("a-credential").is_err());
    }

    #[test]
    fn adc_never_reaches_the_metadata_server_or_the_gcloud_binary() {
        assert_eq!(
            steps(&CredentialSource::ApplicationDefault),
            &[Step::EnvServiceAccount, Step::UserCredentialsFile]
        );
        assert_eq!(steps(&CredentialSource::MetadataServer), &[Step::Metadata]);
        assert_eq!(steps(&CredentialSource::GcloudCli), &[Step::Gcloud]);
    }

    #[test]
    fn the_metadata_and_gcloud_sources_are_named_not_inferred() {
        assert_eq!(
            CredentialSource::parse("Metadata"),
            CredentialSource::MetadataServer
        );
        assert_eq!(
            CredentialSource::parse("gcloud"),
            CredentialSource::GcloudCli
        );
        assert_eq!(
            CredentialSource::parse("./adc"),
            CredentialSource::ServiceAccountFile("./adc".into())
        );
        for source in [
            CredentialSource::ApplicationDefault,
            CredentialSource::MetadataServer,
            CredentialSource::GcloudCli,
        ] {
            assert_eq!(CredentialSource::parse(&source.as_setting()), source);
        }
    }

    #[test]
    fn the_multi_regions_have_their_own_hosts() {
        let host = |region: &str| {
            VertexSettings::new(CredentialSource::ApplicationDefault, "p", region)
                .expect("settings")
                .api_host()
        };
        assert_eq!(host("us"), "aiplatform.us.rep.googleapis.com");
        assert_eq!(host("eu"), "aiplatform.eu.rep.googleapis.com");
        assert_eq!(host("global"), "aiplatform.googleapis.com");
        assert_eq!(host("us-central1"), "us-central1-aiplatform.googleapis.com");
    }

    /// A key body of `lines` distinct base64-looking lines, like a real PEM.
    fn fake_key(lines: usize) -> String {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut key = String::from("-----BEGIN PRIVATE KEY-----\n");
        for _ in 0..lines {
            for _ in 0..64 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                key.push(alphabet[(state % 64) as usize] as char);
            }
            key.push('\n');
        }
        key.push_str("-----END PRIVATE KEY-----\n");
        key
    }

    #[test]
    fn a_key_file_gives_its_secrets_in_the_forms_a_tool_could_print() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("sa.json");
        let key = fake_key(3);
        let file = serde_json::json!({
            "type": "service_account",
            "project_id": "p",
            "private_key_id": "0123456789abcdef0123456789abcdef01234567",
            "private_key": key,
            "client_email": "sa@p.iam.gserviceaccount.com",
        });
        std::fs::write(&path, file.to_string()).expect("write");
        let secrets =
            credential_file_secrets(&CredentialSource::ServiceAccountFile(path), &|_| None);
        let values: Vec<&str> = secrets.iter().map(|(_, v)| v.as_str()).collect();
        // The value as decoded, with real line breaks.
        assert!(values.contains(&key.trim()));
        // The value as the file spells it, with `\n` in place of each break.
        assert!(values.contains(&key.trim().replace('\n', "\\n").as_str()));
        // Each body line, and not the armor lines.
        for line in key.lines().filter(|l| !l.starts_with("-----")) {
            assert!(values.contains(&line), "body line missing");
        }
        assert!(
            !values
                .iter()
                .any(|v| v.starts_with("-----") && v.len() < 40)
        );
        assert!(values.contains(&"0123456789abcdef0123456789abcdef01234567"));
        // The email and project are not secrets.
        assert!(!values.iter().any(|v| v.contains("iam.gserviceaccount")));
    }

    #[test]
    fn a_user_credential_gives_its_refresh_token_and_client_secret() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("adc.json");
        std::fs::write(
            &path,
            r#"{"type":"authorized_user","client_id":"c.apps.googleusercontent.com",
                "client_secret":"example-client-secret-not-real",
                "refresh_token":"1//0gExampleRefreshTokenValue-abcdefghij",
                "quota_project_id":"my-billing-project"}"#,
        )
        .expect("write");
        let path_text = path.to_string_lossy().into_owned();
        let lookup =
            move |name: &str| (name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| path_text.clone());
        let secrets = credential_file_secrets(&CredentialSource::ApplicationDefault, &lookup);
        let kinds: Vec<&str> = secrets.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&"vertex-refresh-token"));
        assert!(kinds.contains(&"vertex-client-secret"));
        assert_eq!(
            quota_project(&CredentialSource::ApplicationDefault, &lookup).as_deref(),
            Some("my-billing-project")
        );
    }

    #[test]
    fn only_a_user_credential_names_a_quota_project() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("sa.json");
        std::fs::write(
            &path,
            r#"{"type":"service_account","project_id":"p","quota_project_id":"other"}"#,
        )
        .expect("write");
        let path_text = path.to_string_lossy().into_owned();
        let lookup =
            move |name: &str| (name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| path_text.clone());
        assert_eq!(
            quota_project(&CredentialSource::ApplicationDefault, &lookup),
            None
        );
        assert_eq!(
            quota_project(&CredentialSource::MetadataServer, &lookup),
            None
        );
        assert!(credential_file_secrets(&CredentialSource::GcloudCli, &lookup).is_empty());
    }

    #[test]
    fn a_missing_key_file_fails_when_the_adapter_is_built() {
        let error = GoogleAuth::new(
            "test",
            CredentialSource::ServiceAccountFile("/nonexistent/sa.json".into()),
            Duration::from_secs(1),
        )
        .err()
        .expect("must fail loud");
        assert!(matches!(error, ProviderError::Decode { .. }), "{error:?}");
        assert!(error.to_string().contains("/nonexistent/sa.json"));
    }

    #[test]
    fn a_key_file_that_is_not_a_key_fails_when_the_adapter_is_built() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("sa.json");
        std::fs::write(&path, "{}").expect("write");
        assert!(
            GoogleAuth::new(
                "test",
                CredentialSource::ServiceAccountFile(path),
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
