//! The worker's configuration file: the MCP servers a worker connects to, the
//! paths whose hard-linked files it may read, the build and test commands its
//! shell may run without approval, and its shell timeout.
//!
//! JSON, because [`McpServerConfig`] is already a validated serde type in JSON
//! (its tests and its redaction are JSON), and a second format for one file
//! would be a second parser to keep in step. The file is read, never written.
//!
//! ```json
//! {
//!   "mcp_servers": [
//!     {"name": "fs", "transport": {"type": "stdio", "command": "mcp-fs", "args": ["--root", "."]}},
//!     {"name": "github", "timeout_ms": 30000,
//!      "transport": {"type": "streamable_http", "url": "https://mcp.example.com/mcp",
//!                    "headers": {"Authorization": {"env": "GITHUB_MCP_TOKEN",
//!                                                  "prefix": "Bearer "},
//!                                "X-Api-Version": {"env": "GITHUB_MCP_API_VERSION",
//!                                                  "secret": false}},
//!                    "proxy": "http://proxy.corp:3128"}},
//!     {"name": "linear",
//!      "transport": {"type": "streamable_http", "url": "https://mcp.linear.example/mcp",
//!                    "oauth": true}}
//!   ],
//!   "hardlink_read_allowed": ["vendor/blobs"],
//!   "build_commands": ["cargo build", "cargo check", "cargo test", "cargo clippy",
//!                      "cargo fmt --check"],
//!   "shell_timeout_secs": 600
//! }
//! ```
//!
//! **`build_commands` (decision 0084).** The build and test commands a worker's
//! shell runs without approval, inside the sandbox with the network off; a
//! worker with a declared write scope runs these and no other command that can
//! write. Each entry is one of `cargo build`, `cargo check`, `cargo test`,
//! `cargo clippy` or `cargo fmt --check`, held to that subcommand's argument
//! rules (`kgraf_tools::execpolicy::BuildAllowlist`); any other entry fails the
//! load. Absent means those five; `[]` means none. An orchestrated run also
//! allows its own acceptance command (`--verify`) and each configured
//! `--gate-command`, exactly as configured ([`WorkerConfig::with_configured_commands`]).
//!
//! **`shell_timeout_secs` (decision 0084).** How long one shell command may run
//! before its process group is killed; its output so far is kept. Absent means
//! 600 (`kgraf_tools::shell::DEFAULT_TIMEOUT`, the contract checks' own bound).
//! A `--shell-timeout` flag overrides it for one invocation.
//!
//! A header value is read from the named environment variable when the server
//! connects; a literal value in the file is refused. `secret` says whether the
//! value is a credential: it defaults to `true` for credential-like names
//! (`Authorization`, `X-Api-Key`, ...) and must be written for any other name. A
//! secret is removed from everything the server sends back and must be at least 8
//! characters. `proxy` is optional; `HTTP_PROXY` and the like are ignored.
//! `oauth: true` marks a server that authorizes with OAuth; any other server that
//! answers 401 fails, and OAuth is never tried on a guess.
//!
//! **Migrating an older file.** A file written before these rules
//! fails to load, with a message naming the entry. Per HTTP server:
//!
//! 1. Move each literal header value into an environment variable, and replace
//!    the value with `{"env": "VARIABLE"}`, with `"prefix": "Bearer "` if the
//!    value started with one. For example `"Authorization": "Bearer ghp_x"`
//!    becomes `"Authorization": {"env": "GITHUB_MCP_TOKEN", "prefix": "Bearer "}`
//!    with `GITHUB_MCP_TOKEN=ghp_x` in the environment kGraf runs in.
//! 2. Every header now says whether it is a credential. A header named like one
//!    (`Authorization`, `X-Api-Key`, `X-Auth-Token`, ...) is a secret by
//!    default. Any other header needs `"secret": false` (or `"secret": true`); the
//!    load error names the header and gives both spellings. A secret shorter
//!    than 8 characters fails at connect with the same advice.
//! 3. For a server that authorized with OAuth (it used to be tried on any 401),
//!    add `"oauth": true`, and remove any configured `Authorization` header.
//! 4. Move a credential out of the URL query string (`?api_key=...`) into a
//!    header; such a URL is now refused.
//! 5. If the server was reached through `HTTP_PROXY`/`HTTPS_PROXY`, name that
//!    proxy in `"proxy"`.
//!
//! **Validated as a whole, at load.** Each server validates itself as it
//! deserializes (unknown keys refused, plain HTTP only to loopback, header
//! injection refused; `kgraf-mcp`'s `config` module); the file then checks
//! what no single entry can: two servers whose names differ only in case, and a
//! hard-link path that is absolute or climbs out of the workspace. Unknown
//! top-level keys are refused, so a misspelled `mcp_server` is an error, not an
//! empty list.
//!
//! **Where it lives.** By default `.kgraf/worker.json` in the repository the
//! worker is dispatched from, never inside the worker's own worktree: a file the
//! worker could write would let it add a server or allow itself a read.

use kgraf_mcp::{McpConfigError, McpServerConfig, validate_all};
use kgraf_tools::execpolicy::{BuildAllowlist, UnknownBuildCommand};
use kgraf_tools::{AccessError, AllowedHardlinkReads};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A configuration that cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum WorkerConfigError {
    #[error("cannot read the worker configuration {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the worker configuration {path} is not valid")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("the worker configuration {path} names its MCP servers inconsistently")]
    Servers {
        path: PathBuf,
        #[source]
        source: McpConfigError,
    },
    #[error("the worker configuration {path} has an unusable hardlink_read_allowed entry")]
    HardlinkPath {
        path: PathBuf,
        #[source]
        source: AccessError,
    },
    #[error("the worker configuration {path} has an unusable build_commands entry")]
    BuildCommand {
        path: PathBuf,
        #[source]
        source: UnknownBuildCommand,
    },
    #[error(
        "the worker configuration {path} sets shell_timeout_secs to 0; give a number of seconds above 0"
    )]
    ShellTimeout { path: PathBuf },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkerConfig {
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    hardlink_read_allowed: Vec<PathBuf>,
    /// `None` (absent) is the cargo default; `Some([])` is none.
    #[serde(default)]
    build_commands: Option<Vec<String>>,
    #[serde(default)]
    shell_timeout_secs: Option<u64>,
}

/// `kgraf worker`'s default number of model rounds, including the final
/// answer round. The command's `--budget` flag and the brief bench both read
/// it, so a bench attempt runs a worker as the command does.
pub const DEFAULT_STEP_BUDGET: u32 = crate::limits::DEFAULT_ROUNDS_SINGLE_WORKER;
/// `kgraf worker`'s default output ceiling per round (`--max-tokens`).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = crate::limits::DEFAULT_WORKER_MAX_TOKENS;
/// `kgraf worker`'s default model context window in tokens (`--window`).
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = crate::limits::DEFAULT_WINDOW;

/// A validated worker configuration.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    mcp_servers: Vec<McpServerConfig>,
    hardlink_reads: AllowedHardlinkReads,
    /// Decision 0084: what the shell runs without approval.
    build_allowlist: BuildAllowlist,
    /// Decision 0084: how long one shell command may run.
    shell_timeout: Duration,
    /// Decision 0093: programs granted to one contract, within the operator's
    /// ceiling. Never read from the file: set per attempt by the host.
    granted_programs: Vec<String>,
}

/// No MCP servers, no hard-linked reads, the cargo build and test allowlist,
/// and the 600 s shell timeout: what a project without a `worker.json` gets.
impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            mcp_servers: Vec::new(),
            hardlink_reads: AllowedHardlinkReads::default(),
            build_allowlist: BuildAllowlist::cargo_default(),
            shell_timeout: kgraf_tools::shell::DEFAULT_TIMEOUT,
            granted_programs: Vec::new(),
        }
    }
}

impl WorkerConfig {
    /// Parse and validate `text`, naming `path` in any error.
    pub fn parse(path: &Path, text: &str) -> Result<Self, WorkerConfigError> {
        let raw: RawWorkerConfig =
            serde_json::from_str(text).map_err(|source| WorkerConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        validate_all(&raw.mcp_servers).map_err(|source| WorkerConfigError::Servers {
            path: path.to_path_buf(),
            source,
        })?;
        let hardlink_reads =
            AllowedHardlinkReads::new(raw.hardlink_read_allowed).map_err(|source| {
                WorkerConfigError::HardlinkPath {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        let build_allowlist = match raw.build_commands {
            None => BuildAllowlist::cargo_default(),
            Some(entries) => entries
                .iter()
                .try_fold(BuildAllowlist::empty(), |list, entry| {
                    BuildAllowlist::parse_cargo_entry(entry)
                        .map(|command| list.with(command))
                        .map_err(|source| WorkerConfigError::BuildCommand {
                            path: path.to_path_buf(),
                            source,
                        })
                })?,
        };
        let shell_timeout = match raw.shell_timeout_secs {
            None => kgraf_tools::shell::DEFAULT_TIMEOUT,
            Some(0) => {
                return Err(WorkerConfigError::ShellTimeout {
                    path: path.to_path_buf(),
                });
            }
            Some(secs) => Duration::from_secs(secs),
        };
        Ok(Self {
            mcp_servers: raw.mcp_servers,
            hardlink_reads,
            build_allowlist,
            shell_timeout,
            granted_programs: Vec::new(),
        })
    }

    /// Read and validate the file at `path`.
    pub fn load(path: &Path) -> Result<Self, WorkerConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| WorkerConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, &text)
    }

    /// The MCP servers every worker of this configuration connects to.
    pub fn mcp_servers(&self) -> &[McpServerConfig] {
        &self.mcp_servers
    }

    /// Paths whose hard-linked files the read tools may read.
    pub fn hardlink_reads(&self) -> &AllowedHardlinkReads {
        &self.hardlink_reads
    }

    /// The build and test commands the shell runs without approval
    /// (decision 0084).
    pub fn build_allowlist(&self) -> &BuildAllowlist {
        &self.build_allowlist
    }

    /// How long one shell command may run before it is killed (decision 0084).
    pub fn shell_timeout(&self) -> Duration {
        self.shell_timeout
    }

    /// Programs granted to this attempt's contract (decision 0093).
    pub fn granted_programs(&self) -> &[String] {
        &self.granted_programs
    }

    /// This configuration with programs granted to one contract after a
    /// worker's permission shortfall (decision 0093). Set by the host from
    /// the contract's record; the file cannot name any.
    pub fn with_granted_programs(mut self, programs: impl IntoIterator<Item = String>) -> Self {
        self.granted_programs.extend(programs);
        self
    }

    /// This configuration with another shell timeout: a `--shell-timeout`
    /// flag, which wins over the file.
    pub fn with_shell_timeout(mut self, timeout: Duration) -> Self {
        self.shell_timeout = timeout;
        self
    }

    /// This configuration with the run's own configured commands (its
    /// acceptance command, its gate commands) added to the build and test
    /// allowlist, each exactly as configured
    /// (`BuildAllowlist::with_configured`). The worker can then run the check
    /// its attempt will be judged by. Added whatever `build_commands` says:
    /// the user configured these commands to run in the worktree already.
    pub fn with_configured_commands<'a>(
        mut self,
        commands: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        for command in commands {
            self.build_allowlist = self.build_allowlist.with_configured(command);
        }
        self
    }

    /// A configuration with these servers and the defaults for everything
    /// else (tests, callers that build servers in code).
    pub fn with_servers(mcp_servers: Vec<McpServerConfig>) -> Result<Self, McpConfigError> {
        validate_all(&mcp_servers)?;
        Ok(Self {
            mcp_servers,
            ..Self::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decisions 0093 and 0094: the worker file cannot raise what a worker
    /// may run: it has no key for a grant, a permission mode, Bypass or plan
    /// review, and an unknown key fails the load rather than being ignored.
    #[test]
    fn the_file_cannot_grant_programs_or_set_the_permission_mode() {
        for text in [
            r#"{"container_sandbox": true}"#,
            r#"{"granted_programs": ["python3"]}"#,
            r#"{"grant_ceiling": ["python3"]}"#,
            r#"{"permission_mode": "bypass"}"#,
            r#"{"mode": "bypass"}"#,
            r#"{"allow_bypass": true}"#,
            r#"{"plan_review": false}"#,
            r#"{"allow": {"programs": ["python3"]}}"#,
            r#"{"script_constructs": ["variables"]}"#,
        ] {
            assert!(
                matches!(parse(text), Err(WorkerConfigError::Parse { .. })),
                "{text}"
            );
        }
        let config = parse("{}").expect("valid");
        assert!(config.granted_programs().is_empty());
    }

    fn parse(text: &str) -> Result<WorkerConfig, WorkerConfigError> {
        WorkerConfig::parse(Path::new("worker.json"), text)
    }

    #[test]
    fn an_older_file_fails_with_errors_that_name_the_fix() {
        let chain = |text: &str| {
            let error = parse(text).expect_err("an older file must not load");
            let mut out = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(next) = source {
                out.push_str(": ");
                out.push_str(&next.to_string());
                source = next.source();
            }
            out
        };
        // A header that is not named like a credential and does not say.
        let missing_secret = chain(
            r#"{"mcp_servers": [{"name": "gh", "transport": {"type": "streamable_http",
                "url": "https://mcp.example.com/mcp",
                "headers": {"X-Api-Version": {"env": "GH_API_VERSION"}}}}]}"#,
        );
        assert!(missing_secret.contains("X-Api-Version"), "{missing_secret}");
        assert!(
            missing_secret.contains(r#""secret": false"#),
            "{missing_secret}"
        );
        // A literal header value.
        let literal = chain(
            r#"{"mcp_servers": [{"name": "gh", "transport": {"type": "streamable_http",
                "url": "https://mcp.example.com/mcp",
                "headers": {"Authorization": "Bearer ghp_x"}}}]}"#,
        );
        assert!(literal.contains(r#"{"env": "VARIABLE""#), "{literal}");
        assert!(!literal.contains("ghp_x"), "{literal}");
    }

    #[test]
    fn the_documented_example_parses() {
        let config = parse(
            r#"{"mcp_servers": [
                {"name": "fs", "transport": {"type": "stdio", "command": "mcp-fs", "args": ["--root", "."]}},
                {"name": "github", "timeout_ms": 30000,
                 "transport": {"type": "streamable_http", "url": "https://mcp.example.com/mcp",
                               "headers": {"Authorization": {"env": "GITHUB_MCP_TOKEN",
                                                             "prefix": "Bearer "},
                                           "X-Api-Version": {"env": "GITHUB_MCP_API_VERSION",
                                                             "secret": false}},
                               "proxy": "http://proxy.corp:3128"}},
                {"name": "linear",
                 "transport": {"type": "streamable_http", "url": "https://mcp.linear.example/mcp",
                               "oauth": true}}
              ],
              "hardlink_read_allowed": ["vendor/blobs"],
              "build_commands": ["cargo build", "cargo check", "cargo test", "cargo clippy",
                                 "cargo fmt --check"],
              "shell_timeout_secs": 600}"#,
        )
        .expect("the example in the module docs must load");
        assert_eq!(config.build_allowlist(), &BuildAllowlist::cargo_default());
        assert_eq!(config.shell_timeout(), Duration::from_secs(600));
        assert_eq!(
            config.mcp_servers()[1].proxy(),
            Some("http://proxy.corp:3128")
        );
        assert!(config.mcp_servers()[2].uses_oauth());
    }

    #[test]
    fn a_full_configuration_parses_and_its_servers_are_validated() {
        let config = parse(
            r#"{"mcp_servers": [
                   {"name": "fs", "transport": {"type": "stdio", "command": "mcp-fs"}},
                   {"name": "gh", "timeout_ms": 5000, "instructions": "use sparingly",
                    "transport": {"type": "streamable_http", "url": "https://mcp.example.com/mcp"}}
               ],
               "hardlink_read_allowed": ["vendor/blobs"]}"#,
        )
        .expect("valid");
        assert_eq!(config.mcp_servers().len(), 2);
        assert_eq!(config.mcp_servers()[1].timeout_ms(), 5000);
        assert!(
            config
                .hardlink_reads()
                .permits(Path::new("vendor/blobs/a.bin"))
        );
    }

    /// Decision 0084: absent `build_commands` is the cargo default, `[]` is
    /// none, and a list is exactly its entries. That these reach the shell is
    /// tested through `assemble` in `kgraf-host`'s tests.
    #[test]
    fn build_commands_parse_absent_empty_and_listed() {
        use kgraf_tools::execpolicy::{BuildCommand, CargoSubcommand};
        assert_eq!(
            parse("{}").expect("valid").build_allowlist(),
            &BuildAllowlist::cargo_default()
        );
        assert!(
            parse(r#"{"build_commands": []}"#)
                .expect("valid")
                .build_allowlist()
                .is_empty()
        );
        let listed =
            parse(r#"{"build_commands": ["cargo check", "cargo fmt --check"]}"#).expect("valid");
        assert_eq!(
            listed.build_allowlist().commands(),
            &[
                BuildCommand::Cargo(CargoSubcommand::Check),
                BuildCommand::Cargo(CargoSubcommand::FmtCheck)
            ]
        );
        let with_run = listed.with_configured_commands(["cargo test --workspace"]);
        assert!(
            with_run
                .build_allowlist()
                .commands()
                .contains(&BuildCommand::Exact(vec![
                    "sh".into(),
                    "-c".into(),
                    "cargo test --workspace".into()
                ]))
        );
    }

    #[test]
    fn an_empty_file_object_is_no_servers_and_no_allowed_reads() {
        let config = parse("{}").expect("valid");
        assert!(config.mcp_servers().is_empty());
        assert!(config.hardlink_reads().paths().is_empty());
    }

    #[test]
    fn every_invalid_shape_is_refused_with_its_reason() {
        for (text, expect) in [
            // A misspelled section is an error, not an empty list.
            (r#"{"mcp_server": []}"#, "Parse"),
            // A server that fails its own validation (plain http to a remote host).
            (
                r#"{"mcp_servers": [{"name": "s", "transport": {"type": "streamable_http", "url": "http://mcp.example.com"}}]}"#,
                "Parse",
            ),
            // A header value written into the file instead of read from the
            // environment.
            (
                r#"{"mcp_servers": [{"name": "s", "transport": {"type": "streamable_http",
                    "url": "https://mcp.example.com", "headers": {"Authorization": "Bearer x"}}}]}"#,
                "Parse",
            ),
            // Names that differ only in case.
            (
                r#"{"mcp_servers": [
                    {"name": "github", "transport": {"type": "stdio", "command": "a"}},
                    {"name": "GitHub", "transport": {"type": "stdio", "command": "b"}}]}"#,
                "Servers",
            ),
            (
                r#"{"hardlink_read_allowed": ["../outside"]}"#,
                "HardlinkPath",
            ),
            (r#"{"hardlink_read_allowed": ["/etc"]}"#, "HardlinkPath"),
            // Decision 0084: only the five cargo spellings are allowlist entries.
            (r#"{"build_commands": ["cargo run"]}"#, "BuildCommand"),
            (r#"{"build_commands": ["cargo fmt"]}"#, "BuildCommand"),
            (r#"{"build_commands": ["make test"]}"#, "BuildCommand"),
            (r#"{"build_commands": "cargo build"}"#, "Parse"),
            (r#"{"shell_timeout_secs": 0}"#, "ShellTimeout"),
            (r#"{"shell_timeout_secs": -1}"#, "Parse"),
        ] {
            let err = parse(text).expect_err(text);
            let kind = match err {
                WorkerConfigError::Parse { .. } => "Parse",
                WorkerConfigError::Servers { .. } => "Servers",
                WorkerConfigError::HardlinkPath { .. } => "HardlinkPath",
                WorkerConfigError::BuildCommand { .. } => "BuildCommand",
                WorkerConfigError::ShellTimeout { .. } => "ShellTimeout",
                WorkerConfigError::Read { .. } => "Read",
            };
            assert_eq!(kind, expect, "{text}");
        }
    }

    /// The refusal of an unknown `build_commands` entry names what the file
    /// accepts, and never offers a form it refuses (an exact argv: those
    /// come only from the run's own acceptance and gate commands).
    #[test]
    fn an_unknown_build_command_names_only_forms_the_file_accepts() {
        let err = parse(r#"{"build_commands": ["make test"]}"#).expect_err("refused");
        let WorkerConfigError::BuildCommand { source, .. } = err else {
            panic!("{err}");
        };
        let message = source.to_string();
        assert!(message.contains("`cargo fmt --check`"), "{message}");
        assert!(!message.contains("exact argv"), "{message}");
    }
}
