//! AWS credentials and region, read from the standard environment variables and
//! the shared config files.
//!
//! The AWS SDK's own provider chain (`aws-config`) covers more sources, but it also
//! reaches the instance metadata service and SSO/STS endpoints, which are network
//! egress the user did not configure, and it pulls in a large dependency tree for a
//! cost path that needs three strings. This module reads only what a user has put on
//! the machine: a Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`), environment
//! variables, then a static-key profile in the shared credentials and config files.
//! A profile that needs a network exchange (role assumption, SSO, a credential
//! process, web identity) is refused with the reason and the command that exports
//! temporary keys, never guessed at.
//!
//! Resolution is repeated on every call. A rotated profile file is picked up, and
//! nothing is held between calls but the last secret registered with the scrubber.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

/// The environment variable that carries a Bedrock API key. Documented at
/// <https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html>: the key
/// is sent as `Authorization: Bearer <key>` on a direct HTTP request to
/// `bedrock-runtime`, in place of a SigV4 signature.
pub const BEARER_TOKEN_VARIABLE: &str = "AWS_BEARER_TOKEN_BEDROCK";

/// The credentials and region one call is signed with.
///
/// `Debug` prints no secret.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsAuth {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: String,
    /// A Bedrock API key. When set the request carries it as a bearer token and is
    /// not signed; the key pair and session token are unused.
    pub bearer_token: Option<String>,
}

impl AwsAuth {
    /// The one secret that stands for these credentials: the API key when there is
    /// one, else the secret access key.
    pub fn primary_secret(&self) -> &str {
        self.bearer_token
            .as_deref()
            .unwrap_or(self.secret_access_key.as_str())
    }
}

impl fmt::Debug for AwsAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsAuth")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .field("region", &self.region)
            .finish()
    }
}

/// Why credentials or a region could not be resolved. Never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AwsConfigError {
    #[error(
        "no AWS credentials found: set AWS_BEARER_TOKEN_BEDROCK (a Bedrock API key), or \
         AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN for temporary \
         keys), or name a profile with static keys in the shared credentials file with \
         AWS_PROFILE"
    )]
    NoCredentials,
    #[error("{name} is set but {missing} is not; both are required")]
    Half {
        name: &'static str,
        missing: &'static str,
    },
    #[error(
        "AWS profile `{profile}` uses `{key}`, which needs a network exchange this adapter \
         does not make; export temporary keys into the environment instead (for example \
         with `aws configure export-credentials --profile {profile} --format env`)"
    )]
    UnsupportedProfile { profile: String, key: &'static str },
    #[error("AWS profile `{profile}` was not found in {file}")]
    ProfileNotFound { profile: String, file: String },
    #[error("cannot read {file}: {reason}")]
    Unreadable { file: String, reason: String },
    #[error("no AWS region: set AWS_REGION (or AWS_DEFAULT_REGION), or `region` in the profile")]
    NoRegion,
    #[error(
        "the AWS region is not valid: it must be lowercase letters, digits and hyphens \
         only, because it becomes part of the host name"
    )]
    BadRegion,
}

/// Source of variables: `std::env::var` in production, a map in tests.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment, treating an empty value as unset.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn get(lookup: Lookup<'_>, name: &str) -> Option<String> {
    lookup(name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// One parsed profile: its keys, lower-cased names to values.
type Profile = HashMap<String, String>;

/// Parse an INI file into sections. Comments start with `#` or `;`. A key may repeat
/// across a section only as a nested block (`s3 =` then indented lines), which is
/// skipped: it never holds a credential key this module reads.
fn parse_ini(text: &str) -> HashMap<String, Profile> {
    let mut sections: HashMap<String, Profile> = HashMap::new();
    let mut current: Option<String> = None;
    // A file saved with a UTF-8 byte order mark would otherwise hide its first
    // section header behind the mark.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for raw in text.lines() {
        let indented = raw.starts_with([' ', '\t']);
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.split(']').next()) {
            current = Some(name.trim().to_owned());
            sections.entry(name.trim().to_owned()).or_default();
            continue;
        }
        if indented {
            // A nested sub-block line; not a top-level key.
            continue;
        }
        if let (Some(section), Some((key, value))) = (&current, line.split_once('=')) {
            sections
                .entry(section.clone())
                .or_default()
                .insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    sections
}

fn home(lookup: Lookup<'_>) -> Option<PathBuf> {
    get(lookup, "HOME")
        .or_else(|| get(lookup, "USERPROFILE"))
        .map(PathBuf::from)
}

fn credentials_path(lookup: Lookup<'_>) -> Option<PathBuf> {
    get(lookup, "AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| home(lookup).map(|h| h.join(".aws").join("credentials")))
}

fn config_path(lookup: Lookup<'_>) -> Option<PathBuf> {
    get(lookup, "AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| home(lookup).map(|h| h.join(".aws").join("config")))
}

/// Read a file into sections; a missing file is empty, any other failure is loud.
fn read_sections(path: Option<PathBuf>) -> Result<HashMap<String, Profile>, AwsConfigError> {
    let Some(path) = path else {
        return Ok(HashMap::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(parse_ini(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(AwsConfigError::Unreadable {
            file: path.display().to_string(),
            reason: e.kind().to_string(),
        }),
    }
}

/// Profile keys that mean the credentials come from a network exchange or another
/// program.
const UNSUPPORTED_KEYS: [&str; 8] = [
    "role_arn",
    "source_profile",
    "credential_source",
    "credential_process",
    "sso_session",
    "sso_start_url",
    "sso_account_id",
    "web_identity_token_file",
];

struct ProfileView {
    name: String,
    credentials: Option<Profile>,
    config: Option<Profile>,
}

impl ProfileView {
    fn load(lookup: Lookup<'_>) -> Result<Self, AwsConfigError> {
        let requested = get(lookup, "AWS_PROFILE");
        let name = requested.clone().unwrap_or_else(|| "default".to_owned());
        let credentials = read_sections(credentials_path(lookup))?.remove(&name);
        // The config file names non-default profiles `profile <name>`.
        let config_key = if name == "default" {
            "default".to_owned()
        } else {
            format!("profile {name}")
        };
        let config = read_sections(config_path(lookup))?.remove(&config_key);
        if requested.is_some() && credentials.is_none() && config.is_none() {
            return Err(AwsConfigError::ProfileNotFound {
                profile: name,
                file: "the shared credentials and config files".to_owned(),
            });
        }
        Ok(Self {
            name,
            credentials,
            config,
        })
    }

    fn value(&self, key: &str) -> Option<String> {
        // The credentials file wins over the config file for the same key.
        [&self.credentials, &self.config]
            .into_iter()
            .flatten()
            .find_map(|p| p.get(key).cloned())
            .filter(|v| !v.trim().is_empty())
    }

    fn refuse_network_sources(&self) -> Result<(), AwsConfigError> {
        for key in UNSUPPORTED_KEYS {
            if self.value(key).is_some() {
                return Err(AwsConfigError::UnsupportedProfile {
                    profile: self.name.clone(),
                    key,
                });
            }
        }
        Ok(())
    }
}

/// Resolve the signing credentials.
///
/// Order: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (with an optional
/// `AWS_SESSION_TOKEN`), then the static keys of the profile named by `AWS_PROFILE`
/// (default: `default`).
pub fn resolve_credentials(
    lookup: Lookup<'_>,
) -> Result<(String, String, Option<String>), AwsConfigError> {
    let id = get(lookup, "AWS_ACCESS_KEY_ID");
    let secret = get(lookup, "AWS_SECRET_ACCESS_KEY");
    match (id, secret) {
        (Some(id), Some(secret)) => return Ok((id, secret, get(lookup, "AWS_SESSION_TOKEN"))),
        (Some(_), None) => {
            return Err(AwsConfigError::Half {
                name: "AWS_ACCESS_KEY_ID",
                missing: "AWS_SECRET_ACCESS_KEY",
            });
        }
        (None, Some(_)) => {
            return Err(AwsConfigError::Half {
                name: "AWS_SECRET_ACCESS_KEY",
                missing: "AWS_ACCESS_KEY_ID",
            });
        }
        (None, None) => {}
    }
    let profile = ProfileView::load(lookup)?;
    profile.refuse_network_sources()?;
    match (
        profile.value("aws_access_key_id"),
        profile.value("aws_secret_access_key"),
    ) {
        (Some(id), Some(secret)) => Ok((id, secret, profile.value("aws_session_token"))),
        (Some(_), None) => Err(AwsConfigError::Half {
            name: "aws_access_key_id",
            missing: "aws_secret_access_key",
        }),
        (None, Some(_)) => Err(AwsConfigError::Half {
            name: "aws_secret_access_key",
            missing: "aws_access_key_id",
        }),
        (None, None) => Err(AwsConfigError::NoCredentials),
    }
}

/// Resolve the region: `AWS_REGION`, `AWS_DEFAULT_REGION`, then the profile's
/// `region`.
pub fn resolve_region(lookup: Lookup<'_>) -> Result<String, AwsConfigError> {
    let region = match get(lookup, "AWS_REGION").or_else(|| get(lookup, "AWS_DEFAULT_REGION")) {
        Some(region) => region,
        None => ProfileView::load(lookup)?
            .value("region")
            .ok_or(AwsConfigError::NoRegion)?,
    };
    validate_region(&region)?;
    Ok(region)
}

/// A region is spliced into the endpoint host name, so anything but lowercase
/// letters, digits and hyphens (a dot, slash, `@` or `:`) is refused.
pub fn validate_region(region: &str) -> Result<(), AwsConfigError> {
    let valid = !region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(AwsConfigError::BadRegion)
    }
}

/// Credentials and region together.
pub fn resolve(lookup: Lookup<'_>) -> Result<AwsAuth, AwsConfigError> {
    // A Bedrock API key wins over signing keys: kGraf's rule, so that one explicit
    // variable decides which credential is used.
    if let Some(token) = get(lookup, BEARER_TOKEN_VARIABLE) {
        return Ok(AwsAuth {
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: None,
            region: resolve_region(lookup)?,
            bearer_token: Some(token),
        });
    }
    let (access_key_id, secret_access_key, session_token) = resolve_credentials(lookup)?;
    Ok(AwsAuth {
        access_key_id,
        secret_access_key,
        session_token,
        region: resolve_region(lookup)?,
        bearer_token: None,
    })
}

/// Register the secret parts of `auth` with the process-wide scrubber, so they are
/// removed from transcripts and error text.
pub fn register_secrets(auth: &AwsAuth) {
    if let Some(token) = &auth.bearer_token {
        kgraf_secrets::register_exact_secret("aws-bedrock-api-key", token);
    }
    if !auth.secret_access_key.is_empty() {
        kgraf_secrets::register_exact_secret("aws-secret-access-key", &auth.secret_access_key);
    }
    if let Some(token) = &auth.session_token {
        kgraf_secrets::register_exact_secret("aws-session-token", token);
    }
}

/// Check that a call could be authenticated, and return the secret that stands for
/// the credentials (the API key, else the secret access key), which is the value the
/// caller registers with its scrubber as this route's credential.
pub fn credential_probe(lookup: Lookup<'_>) -> Result<String, AwsConfigError> {
    resolve(lookup).map(|auth| auth.primary_secret().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Env = Box<dyn Fn(&str) -> Option<String>>;

    fn map(pairs: &[(&str, &str)]) -> Env {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Box::new(move |name: &str| m.get(name).cloned())
    }

    #[test]
    fn environment_keys_and_region_resolve() {
        let env = map(&[
            ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "secret-value"),
            ("AWS_SESSION_TOKEN", "token-value"),
            ("AWS_REGION", "us-west-2"),
        ]);
        let auth = resolve(&env).expect("resolved");
        assert_eq!(auth.access_key_id, "AKIDEXAMPLE");
        assert_eq!(auth.session_token.as_deref(), Some("token-value"));
        assert_eq!(auth.region, "us-west-2");
    }

    #[test]
    fn an_api_key_resolves_without_access_keys_and_wins_over_them() {
        let auth = resolve(&map(&[
            ("AWS_BEARER_TOKEN_BEDROCK", "api-key-value"),
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_REGION", "us-east-1"),
        ]))
        .expect("resolved");
        assert_eq!(auth.bearer_token.as_deref(), Some("api-key-value"));
        assert_eq!(auth.primary_secret(), "api-key-value");
        let text = format!("{auth:?}");
        assert!(!text.contains("api-key-value"), "{text}");
        // The region is still needed: it picks the host.
        let err = resolve(&map(&[
            ("AWS_BEARER_TOKEN_BEDROCK", "api-key-value"),
            ("HOME", "/nonexistent-home-for-test"),
        ]))
        .expect_err("no region");
        assert_eq!(err, AwsConfigError::NoRegion);
    }

    #[test]
    fn no_credentials_message_names_the_api_key_variable() {
        let text = AwsConfigError::NoCredentials.to_string();
        assert!(text.contains("AWS_BEARER_TOKEN_BEDROCK"), "{text}");
    }

    #[test]
    fn a_region_with_host_characters_is_refused() {
        for bad in [
            "evil.example.com",
            "a/b",
            "us-east-1@x",
            "US-EAST-1",
            "a:80",
        ] {
            let err = resolve_region(&map(&[("AWS_REGION", bad)])).expect_err(bad);
            assert_eq!(err, AwsConfigError::BadRegion, "{bad}");
        }
        assert_eq!(
            resolve_region(&map(&[("AWS_REGION", "us-gov-west-1")])).expect("ok"),
            "us-gov-west-1"
        );
    }

    #[test]
    fn debug_never_prints_a_secret() {
        let env = map(&[
            ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "secret-value"),
            ("AWS_SESSION_TOKEN", "token-value"),
            ("AWS_REGION", "us-west-2"),
        ]);
        let text = format!("{:?}", resolve(&env).expect("resolved"));
        for secret in ["AKIDEXAMPLE", "secret-value", "token-value"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[test]
    fn half_a_key_pair_raises_and_names_both_variables() {
        let err = resolve(&map(&[("AWS_ACCESS_KEY_ID", "AKID")])).expect_err("half");
        assert!(err.to_string().contains("AWS_SECRET_ACCESS_KEY"), "{err}");
        let err = resolve(&map(&[("AWS_SECRET_ACCESS_KEY", "s")])).expect_err("half");
        assert!(err.to_string().contains("AWS_ACCESS_KEY_ID"), "{err}");
    }

    #[test]
    fn nothing_configured_raises_and_says_what_to_set() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().to_string_lossy().into_owned();
        let err = resolve_credentials(&map(&[("HOME", &home)])).expect_err("none");
        assert_eq!(err, AwsConfigError::NoCredentials);
        assert!(err.to_string().contains("AWS_ACCESS_KEY_ID"));
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_section() {
        let text = "\u{feff}[default]\naws_access_key_id = AKID\naws_secret_access_key = s\n";
        let sections = parse_ini(text);
        assert_eq!(
            sections
                .get("default")
                .and_then(|p| p.get("aws_access_key_id"))
                .map(String::as_str),
            Some("AKID")
        );
    }

    #[test]
    fn a_static_profile_supplies_keys_and_region() {
        let dir = tempfile::tempdir().expect("dir");
        let creds = dir.path().join("credentials");
        let config = dir.path().join("config");
        std::fs::write(
            &creds,
            "# comment\n[default]\naws_access_key_id = DEFAULTID\naws_secret_access_key = defaultsecret\n\n\
             [work]\naws_access_key_id = WORKID\naws_secret_access_key = worksecret\naws_session_token = worktoken\n",
        )
        .expect("write");
        std::fs::write(
            &config,
            "[default]\nregion = eu-west-1\n\n[profile work]\nregion = ap-southeast-2\ns3 =\n  max_concurrent_requests = 5\n",
        )
        .expect("write");
        let base = [
            (
                "AWS_SHARED_CREDENTIALS_FILE",
                creds.to_string_lossy().into_owned(),
            ),
            ("AWS_CONFIG_FILE", config.to_string_lossy().into_owned()),
        ];
        let pairs = |profile: Option<&str>| {
            let mut v: Vec<(&str, String)> = base.iter().map(|(k, v)| (*k, v.clone())).collect();
            if let Some(p) = profile {
                v.push(("AWS_PROFILE", p.to_owned()));
            }
            v
        };
        let lookup = |profile: Option<&str>| {
            let m: HashMap<String, String> = pairs(profile)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
            move |name: &str| m.get(name).cloned()
        };
        let default = resolve(&lookup(None)).expect("default");
        assert_eq!(default.access_key_id, "DEFAULTID");
        assert_eq!(default.region, "eu-west-1");
        let work = resolve(&lookup(Some("work"))).expect("work");
        assert_eq!(work.secret_access_key, "worksecret");
        assert_eq!(work.session_token.as_deref(), Some("worktoken"));
        assert_eq!(work.region, "ap-southeast-2");
        let err = resolve(&lookup(Some("nope"))).expect_err("missing profile");
        assert!(
            matches!(err, AwsConfigError::ProfileNotFound { .. }),
            "{err}"
        );
    }

    #[test]
    fn environment_wins_over_a_profile() {
        let dir = tempfile::tempdir().expect("dir");
        let creds = dir.path().join("credentials");
        std::fs::write(
            &creds,
            "[default]\naws_access_key_id = FILEID\naws_secret_access_key = filesecret\n",
        )
        .expect("write");
        let file = creds.to_string_lossy().into_owned();
        let env = map(&[
            ("AWS_SHARED_CREDENTIALS_FILE", &file),
            ("AWS_ACCESS_KEY_ID", "ENVID"),
            ("AWS_SECRET_ACCESS_KEY", "envsecret"),
        ]);
        assert_eq!(resolve_credentials(&env).expect("env").0, "ENVID");
    }

    #[test]
    fn a_profile_that_needs_a_network_exchange_is_refused_with_the_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("config");
        std::fs::write(
            &config,
            "[profile sso]\nsso_session = corp\nsso_account_id = 1\nregion = us-east-1\n",
        )
        .expect("write");
        let file = config.to_string_lossy().into_owned();
        let home = dir.path().to_string_lossy().into_owned();
        let env = map(&[
            ("AWS_CONFIG_FILE", &file),
            ("AWS_PROFILE", "sso"),
            ("HOME", &home),
        ]);
        let err = resolve_credentials(&env).expect_err("refused");
        match &err {
            AwsConfigError::UnsupportedProfile { key, .. } => assert_eq!(*key, "sso_session"),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(err.to_string().contains("export-credentials"), "{err}");
    }

    #[test]
    fn a_missing_region_raises() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().to_string_lossy().into_owned();
        let err = resolve_region(&map(&[("HOME", &home)])).expect_err("none");
        assert_eq!(err, AwsConfigError::NoRegion);
        let default_region = resolve_region(&map(&[("AWS_DEFAULT_REGION", "us-east-2")]));
        assert_eq!(default_region.expect("region"), "us-east-2");
    }

    #[test]
    fn a_blank_variable_counts_as_unset() {
        let env = map(&[
            ("AWS_ACCESS_KEY_ID", "  "),
            ("AWS_SECRET_ACCESS_KEY", ""),
            ("HOME", "/nonexistent-home-for-test"),
        ]);
        assert_eq!(
            resolve_credentials(&env).expect_err("blank"),
            AwsConfigError::NoCredentials
        );
    }
}
