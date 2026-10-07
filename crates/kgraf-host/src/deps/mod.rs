//! Dependency fetching with the user's per-project consent (decision 0087).
//!
//! Workers build with the network off and cannot write the cargo cache
//! (decision 0084), so a run needs its dependencies downloaded and unpacked
//! before it starts. At run start the host checks that they are; when they
//! are not, it fetches them only with the user's consent for this project.
//! The fetch is the one network access kGraf makes that is not to a model
//! provider or an MCP server the user configured.
//!
//! ```text
//!   run start --> project uses cargo? --no--> go on (no check)
//!                    | yes
//!                    v
//!               offline check --ready--> go on
//!                    | missing
//!                    v
//!               this run's answer (flag, desktop "Allow once") over the
//!               stored consent (unset / allowed / refused)
//!                    |-- allow --> fetch on the host --> check again --ready--> go on
//!                    |                                          `--missing--> fail loud
//!                    |-- unset --> fail loud: ConsentNeeded (a caller that can ask, asks,
//!                    |             then starts again)
//!                    `-- refuse -> fail loud: Refused
//! ```
//!
//! The check and the fetch never run on the project's own files: they run on
//! a mirror of its manifests in a private folder under the checkout's state
//! directory ([`cargo`]), so
//! cargo writes no lockfile into the user's checkout, and a worker's worktree
//! (which a worker controls) is never written or followed through a link by
//! a host process. `cargo fetch` downloads and unpacks; it runs no build
//! script (measured: a root and a path dependency with `build.rs` each, no
//! script ran and no `target` directory was made).

mod cargo;
#[cfg(unix)]
mod group;
pub mod install;
pub mod installs;
mod refetch;

pub use cargo::{CargoDeps, FetchSite};
pub use install::{
    InstallHosts, InstallOutcome, InstallReport, InstallRunner, SandboxedInstall, ensure_installs,
};
pub use installs::{Ecosystem, InstallEnv, MissingInstall, missing_installs};
/// An allowed-host entry of an install (`kgraf_sandbox::egress`).
pub use kgraf_sandbox::egress::HostRule;
pub use refetch::{HostRefetch, MAX_WORKER_CHECKS, MAX_WORKER_FETCHES};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A request to stop a running fetch: the fetch's child process is killed
/// the next time it is polled (every few milliseconds). A [`Stop::child`]
/// is stopped by its own flag or its parent's, so one run-wide stop reaches
/// every fetch the run's workers started, and one call's stop reaches only
/// that call's fetch.
#[derive(Clone, Debug, Default)]
pub struct Stop {
    own: Arc<AtomicBool>,
    parent: Option<Arc<AtomicBool>>,
}

impl Stop {
    /// A stop linked to this one: stopped when either is.
    pub fn child(&self) -> Self {
        Self {
            own: Arc::new(AtomicBool::new(false)),
            parent: Some(self.own.clone()),
        }
    }

    pub fn stop(&self) {
        self.own.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.own.load(Ordering::SeqCst)
            || self
                .parent
                .as_ref()
                .is_some_and(|p| p.load(Ordering::SeqCst))
    }
}

/// The file, in the project's state directory (`.kgraf`), that holds the
/// consent. Not in the repository: `.kgraf` is excluded from git, and a
/// worker's sandbox cannot read or write it.
pub const CONSENT_FILE: &str = "dependency-fetch.json";

/// A package registry kGraf can fetch from. Only crates.io is built; npm and
/// PyPI would be further variants with their own [`DependencyTool`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Registry {
    #[serde(rename = "crates.io")]
    CratesIo,
}

impl Registry {
    pub fn name(self) -> &'static str {
        match self {
            Registry::CratesIo => "crates.io",
        }
    }

    /// The command a user runs to fetch by hand.
    pub fn manual_command(self) -> &'static str {
        match self {
            Registry::CratesIo => "cargo fetch",
        }
    }

    /// The name `kgraf deps` takes for it.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "crates.io" | "cargo" => Some(Registry::CratesIo),
            _ => None,
        }
    }
}

impl std::fmt::Display for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The stored answer for one registry in one project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    /// Never asked, or reset: a run that needs a fetch stops and asks.
    #[default]
    Unset,
    /// Fetch whenever a run needs it, without asking again.
    Allowed,
    /// Never fetch; a run that needs a fetch fails with the instruction.
    Refused,
}

impl Consent {
    pub fn name(self) -> &'static str {
        match self {
            Consent::Unset => "unset",
            Consent::Allowed => "allowed",
            Consent::Refused => "refused",
        }
    }
}

/// This run's own answer, which wins over the stored one: the command's
/// `--fetch-deps` / `--no-fetch-deps`, or the desktop's "Allow once".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunFetch {
    /// No answer for this run: the stored consent decides.
    #[default]
    Stored,
    /// Fetch for this run (at its start and when a worker needs it), storing nothing.
    Allow,
    /// Do not fetch for this run, storing nothing.
    Refuse,
}

/// The consent file of one project.
#[derive(Clone, Debug)]
pub struct ConsentStore {
    path: PathBuf,
    /// The key that seals each install answer ([`InstallConsent::seal`]).
    key: ConsentKey,
}

/// `KGRAF_CONSENT_KEY_PATH`: the file holding the key that seals install
/// answers, instead of the per-user one ([`ConsentKey::user_path`]).
pub const CONSENT_KEY_PATH_ENV: &str = "KGRAF_CONSENT_KEY_PATH";

/// Where the key that seals install answers comes from.
///
/// **Why a seal.** The consent file lives in the project's `.kgraf`, and a
/// project's own files are not the user's word: a repository can commit
/// `.kgraf/dependency-fetch.json`, and an archive can unpack one, with an
/// install "allowed" for the very command kGraf would run. An install answer
/// therefore counts only with a seal kGraf made: a keyed BLAKE3 hash of the
/// answer under a random key kept per user, outside every project, in
/// kGraf's own data folder (which the sandbox denies to every command on
/// macOS and never grants on Linux). An answer without a valid seal reads as
/// unset, and the run asks.
#[derive(Clone, Debug)]
enum ConsentKey {
    /// The per-user key file, made on first use.
    User,
    /// A given key (tests; a caller with its own key).
    Fixed([u8; kgraf_diag::key::KEY_LEN]),
}

impl Default for ConsentKey {
    fn default() -> Self {
        // This crate's unit tests never touch the user's data folder.
        if cfg!(test) {
            ConsentKey::Fixed(*b"kgraf-host-test-consent-key-32b!")
        } else {
            ConsentKey::User
        }
    }
}

impl ConsentKey {
    /// The per-user key file: [`CONSENT_KEY_PATH_ENV`], else
    /// `install-consent.key` in kGraf's data folder.
    fn user_path() -> Option<PathBuf> {
        match std::env::var_os(CONSENT_KEY_PATH_ENV) {
            Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
            _ => directories::ProjectDirs::from("", "", "kGraf")
                .map(|d| d.data_dir().join("install-consent.key")),
        }
    }

    fn load(&self) -> Result<[u8; kgraf_diag::key::KEY_LEN], DepsError> {
        match self {
            ConsentKey::Fixed(k) => Ok(*k),
            ConsentKey::User => {
                let path = Self::user_path().ok_or_else(|| DepsError::Store {
                    path: PathBuf::from("install-consent.key"),
                    detail: "no per-user data folder is known for the key that seals install \
                             answers"
                        .into(),
                })?;
                kgraf_diag::key::load_or_create(&path).map_err(|e| {
                    DepsError::io(
                        format!(
                            "cannot read or make the install answer key {}",
                            path.display()
                        ),
                        e,
                    )
                })
            }
        }
    }
}

/// The seal of an install answer: the keyed hash of its key, command and
/// answer, so a seal made for one cannot be moved to another.
fn install_seal(
    key: &[u8; kgraf_diag::key::KEY_LEN],
    consent_key: &str,
    command: &str,
    consent: Consent,
) -> blake3::Hash {
    let mut h = blake3::Hasher::new_keyed(key);
    for part in [
        "kgraf install answer 1",
        consent_key,
        command,
        consent.name(),
    ] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.finalize()
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentFile {
    version: u32,
    #[serde(default)]
    registries: BTreeMap<Registry, Consent>,
    /// Version 2: the answer about each install command kGraf may run in its
    /// sandbox, keyed by [`MissingInstall::consent_key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    installs: BTreeMap<String, InstallConsent>,
}

/// The stored answer about one install command. It holds for that exact
/// command only: when the project's install command changes (a lockfile
/// added, a different package manager), the answer no longer applies and a
/// run asks again. A changed lockfile or package script does not change the
/// command: what it runs, it runs in the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallConsent {
    pub command: String,
    pub consent: Consent,
    /// kGraf's seal of this answer, hex ([`ConsentKey`]); an answer without
    /// a valid one reads as unset.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seal: String,
}

/// The version a file with no install answers is written as, so an older
/// kGraf still reads it.
const CONSENT_VERSION: u32 = 1;
/// The version a file with install answers is written as.
const CONSENT_VERSION_INSTALLS: u32 = 2;

impl ConsentStore {
    /// The store in the state directory `state` (`<project>/.kgraf`).
    pub fn in_state_dir(state: &Path) -> Self {
        Self {
            path: state.join(CONSENT_FILE),
            key: ConsentKey::default(),
        }
    }

    /// This store, sealing and checking install answers with `key` instead
    /// of the per-user key (another crate's tests, which must not touch the
    /// user's data folder).
    pub fn with_key(mut self, key: [u8; kgraf_diag::key::KEY_LEN]) -> Self {
        self.key = ConsentKey::Fixed(key);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<ConsentFile, DepsError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ConsentFile::default()),
            Err(e) => {
                return Err(DepsError::io(
                    format!("cannot read {}", self.path.display()),
                    e,
                ));
            }
        };
        // A damaged file is an error, never read as "unset": that would ask
        // again, or worse, hide a refusal the user stored.
        let file: ConsentFile = serde_json::from_str(&text).map_err(|e| DepsError::Store {
            path: self.path.clone(),
            detail: e.to_string(),
        })?;
        if file.version != CONSENT_VERSION && file.version != CONSENT_VERSION_INSTALLS {
            return Err(DepsError::Store {
                path: self.path.clone(),
                detail: format!(
                    "version {} is not {CONSENT_VERSION} or {CONSENT_VERSION_INSTALLS}, the ones \
                     this kGraf reads",
                    file.version
                ),
            });
        }
        Ok(file)
    }

    /// The stored answer about running `install`: [`Consent::Unset`] when
    /// none is stored, when the stored one was given for another command, or
    /// when kGraf did not seal it (a file a repository or an archive brought,
    /// [`ConsentKey`]).
    pub fn install_consent(&self, install: &MissingInstall) -> Result<Consent, DepsError> {
        let file = self.read()?;
        let key = install.consent_key();
        let Some(answer) = file
            .installs
            .get(&key)
            .filter(|c| c.command == install.command && c.consent != Consent::Unset)
        else {
            return Ok(Consent::Unset);
        };
        let Ok(stored) = blake3::Hash::from_hex(&answer.seal) else {
            return Ok(Consent::Unset);
        };
        // `blake3::Hash` compares in constant time.
        let expected = install_seal(&self.key.load()?, &key, &answer.command, answer.consent);
        Ok(if stored == expected {
            answer.consent
        } else {
            Consent::Unset
        })
    }

    /// Store `consent` for running `install` (its exact command); `Unset`
    /// removes the entry.
    pub fn set_install(&self, install: &MissingInstall, consent: Consent) -> Result<(), DepsError> {
        let mut file = self.read()?;
        match consent {
            Consent::Unset => {
                file.installs.remove(&install.consent_key());
            }
            c => {
                let key = install.consent_key();
                let seal = install_seal(&self.key.load()?, &key, &install.command, c);
                file.installs.insert(
                    key,
                    InstallConsent {
                        command: install.command.clone(),
                        consent: c,
                        seal: seal.to_hex().to_string(),
                    },
                );
            }
        }
        self.write(file)
    }

    /// Remove every stored install answer of `ecosystem`.
    pub fn reset_installs(&self, ecosystem: Ecosystem) -> Result<usize, DepsError> {
        let mut file = self.read()?;
        let prefix = format!("{}:", ecosystem.key());
        let before = file.installs.len();
        file.installs.retain(|k, _| !k.starts_with(&prefix));
        let removed = before - file.installs.len();
        self.write(file)?;
        Ok(removed)
    }

    /// Every stored install answer, by key.
    pub fn installs(&self) -> Result<BTreeMap<String, InstallConsent>, DepsError> {
        Ok(self.read()?.installs)
    }

    /// The stored consent for `registry`; [`Consent::Unset`] when none is stored.
    pub fn get(&self, registry: Registry) -> Result<Consent, DepsError> {
        Ok(self
            .read()?
            .registries
            .get(&registry)
            .copied()
            .unwrap_or_default())
    }

    /// Store `consent` for `registry` (`Unset` removes the entry). Written to
    /// a temporary file beside it and renamed over it, so a crash leaves the
    /// old file or the new one, never half of one.
    pub fn set(&self, registry: Registry, consent: Consent) -> Result<(), DepsError> {
        let mut file = self.read()?;
        match consent {
            Consent::Unset => file.registries.remove(&registry),
            c => file.registries.insert(registry, c),
        };
        self.write(file)
    }

    fn write(&self, mut file: ConsentFile) -> Result<(), DepsError> {
        file.version = if file.installs.is_empty() {
            CONSENT_VERSION
        } else {
            CONSENT_VERSION_INSTALLS
        };
        let dir = self.path.parent().ok_or_else(|| DepsError::Store {
            path: self.path.clone(),
            detail: "has no parent directory".into(),
        })?;
        let text = serde_json::to_string_pretty(&file).map_err(|e| DepsError::Store {
            path: self.path.clone(),
            detail: e.to_string(),
        })?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .map_err(|e| DepsError::io(format!("cannot write in {}", dir.display()), e))?;
        std::io::Write::write_all(&mut tmp, format!("{text}\n").as_bytes())
            .map_err(|e| DepsError::io(format!("cannot write {}", self.path.display()), e))?;
        tmp.persist(&self.path).map_err(|e| {
            DepsError::io(format!("cannot replace {}", self.path.display()), e.error)
        })?;
        Ok(())
    }
}

/// Whether a project's dependencies are ready for an offline build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// The project does not use this registry's package manager.
    NotApplicable,
    /// Every dependency is downloaded and unpacked.
    Ready,
    /// A dependency is not; the package manager's words.
    Missing(String),
    /// The check failed for another reason (a malformed manifest, say): not a
    /// dependency the fetch could supply. The build will show the same error.
    Unchecked(String),
}

/// What a fetch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchReport {
    pub registry: Registry,
    /// The package manager's own lines naming each index it contacted
    /// ("Updating crates.io index"); the egress, as it said it.
    pub contacted: Vec<String>,
    /// Packages downloaded.
    pub downloaded: usize,
    pub elapsed: Duration,
}

impl FetchReport {
    pub fn summary(&self) -> String {
        let contacted = if self.contacted.is_empty() {
            format!("{} (no index update reported)", self.registry)
        } else {
            self.contacted.join("; ")
        };
        format!(
            "{contacted}; {} package(s) downloaded in {:.1} s",
            self.downloaded,
            self.elapsed.as_secs_f64()
        )
    }
}

/// A registry's package manager, as the gate uses it. [`CargoDeps`] is the
/// real one; tests pass a fake, so no test contacts a registry.
pub trait DependencyTool: Send + Sync {
    fn registry(&self) -> Registry;
    /// Whether the project at `site` uses this package manager at all.
    fn applies(&self, site: &FetchSite) -> bool;
    /// The offline check. Never contacts the network.
    fn check(&self, site: &FetchSite) -> Readiness;
    /// [`Self::check`] that gives up when `stop` is set: a check the run
    /// ended or the build cancelled must not run on to its timeout. A tool
    /// whose check cannot be interrupted keeps this default.
    fn check_watched(&self, site: &FetchSite, _stop: &Stop) -> Readiness {
        self.check(site)
    }
    /// Download and unpack the dependencies, with the network, on the host.
    /// A fetch still running when `stop` is set is killed and fails.
    fn fetch(&self, site: &FetchSite, stop: &Stop) -> Result<FetchReport, DepsError>;
    /// When [`Self::applies`] is false: a manifest of this package manager
    /// below the root, if any (relative to it). The gate then says the
    /// project was not checked instead of passing silently.
    fn nested_manifest(&self, _site: &FetchSite) -> Option<PathBuf> {
        None
    }
}

/// Why the gate stopped a run, or a fetch failed.
#[derive(Debug, thiserror::Error)]
pub enum DepsError {
    /// Dependencies are missing and nobody has answered whether kGraf may
    /// fetch them. A caller that can ask (a terminal, the desktop) asks, then
    /// starts again; an unattended one shows this.
    #[error(
        "the dependencies of {} are not in the local cache, and kGraf has no answer about \
         fetching them from {registry}. Workers build with the network off, so the run cannot \
         start without them. Run `{}` in the project yourself, or let kGraf fetch: `kgraf deps \
         allow` (always, for this project), or `--fetch-deps` (this run only; `--no-fetch-deps` \
         declines). Missing: {detail}",
        project.display(),
        registry.manual_command()
    )]
    ConsentNeeded {
        registry: Registry,
        project: PathBuf,
        detail: String,
    },
    /// Dependencies are missing and fetching is refused.
    #[error(
        "the dependencies of {} are not in the local cache, and fetching from {registry} is \
         refused {}. Run `{}` in the project yourself, or allow kGraf to fetch: {}. Missing: \
         {detail}",
        project.display(),
        if *for_this_run { "for this run (--no-fetch-deps)" } else { "for this project" },
        registry.manual_command(),
        if *for_this_run { "drop --no-fetch-deps" } else { "`kgraf deps allow`" }
    )]
    Refused {
        registry: Registry,
        project: PathBuf,
        detail: String,
        for_this_run: bool,
    },
    /// The manifests name a source other than the registry consented to (a
    /// git repository, another registry): kGraf does not fetch them.
    #[error(
        "kGraf fetches only from {registry}, and {} also names {}. Run `{}` in the project \
         yourself if you trust them",
        project.display(),
        sources.join(", "),
        registry.manual_command()
    )]
    OtherSources {
        registry: Registry,
        project: PathBuf,
        sources: Vec<String>,
    },
    /// The fetch ran and dependencies are still missing.
    #[error("after fetching from {registry}, a dependency is still not ready: {detail}")]
    StillMissing { registry: Registry, detail: String },
    /// The fetch itself failed (the network, a timeout, the package manager).
    #[error("fetching from {registry} failed: {detail}")]
    Fetch { registry: Registry, detail: String },
    /// The project's own shape stops the fetch (too large to mirror, no root
    /// `Cargo.toml`). The same run fails the same way each time.
    #[error("the project cannot be fetched for: {detail}")]
    Project { detail: String },
    /// Installs are missing and nobody has answered whether kGraf may run
    /// their install commands in its sandbox. A caller that can ask asks,
    /// then starts again; an unattended one shows this.
    #[error(
        "{} needs installs that are missing, and kGraf has no answer about running them: {}. \
         Workers run with the network off, so the run cannot start without them. Run each \
         command yourself, or let kGraf run it in its sandbox (writes only in the project, never \
         its .git or .kgraf; network only to the package registries): `kgraf deps allow {}` \
         (always, for these exact commands), or `--fetch-deps` (this run only; \
         `--no-fetch-deps` goes on without them)",
        project.display(),
        install_list(installs),
        install_keys(installs)
    )]
    InstallConsentNeeded {
        project: PathBuf,
        installs: Vec<MissingInstall>,
    },
    /// kGraf ran an install command in its sandbox and it failed.
    #[error(
        "kGraf ran `{}` in {} ({}) in its sandbox, and it failed: {detail}",
        install.command,
        install.place(),
        install.ecosystem.name()
    )]
    InstallFailed {
        install: MissingInstall,
        detail: String,
    },
    /// kGraf ran an install command and its install is still missing.
    #[error(
        "kGraf ran `{}` in {} and it succeeded, but the install is still missing: {what}. \
         Run it yourself to see why",
        install.command,
        install.place()
    )]
    InstallIncomplete {
        install: MissingInstall,
        what: String,
    },
    /// The `install_registries` setting cannot be used.
    #[error("the install registries setting is not usable, so no install was run: {detail}")]
    Settings { detail: String },
    /// The consent file is damaged.
    #[error("{} is not a dependency-fetch consent file: {detail}", path.display())]
    Store { path: PathBuf, detail: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl DepsError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        DepsError::Io {
            context: context.into(),
            source,
        }
    }

    /// Whether the same run could start later unchanged: a failed fetch (the
    /// network) could; a missing answer, a refusal, a damaged file or a
    /// project that cannot be mirrored cannot.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            DepsError::Fetch { .. } | DepsError::Io { .. } | DepsError::InstallFailed { .. }
        )
    }
}

/// "`npm ci` in the project root (npm); ..." for a message.
fn install_list(installs: &[MissingInstall]) -> String {
    installs
        .iter()
        .map(|i| format!("`{}` in {} ({})", i.command, i.place(), i.ecosystem.name()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The distinct ecosystem words of `installs`, for `kgraf deps allow`.
fn install_keys(installs: &[MissingInstall]) -> String {
    let mut keys: Vec<&str> = installs.iter().map(|i| i.ecosystem.key()).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.join(" ")
}

/// What a person is asked to allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsentSubject {
    /// A fetch from a registry, on the host (cargo).
    Fetch(Registry),
    /// These install commands, run in kGraf's sandbox.
    Install(Vec<MissingInstall>),
}

/// The question a person answers when [`DepsError::ConsentNeeded`] or
/// [`DepsError::InstallConsentNeeded`] stops a run: the desktop's prompt,
/// the terminal's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentQuestion {
    pub subject: ConsentSubject,
    pub project: PathBuf,
    /// What is missing, in the package manager's words or kGraf's.
    pub detail: String,
}

/// What a consent covers beyond the run's own start, in one sentence for
/// every place that asks or reports it (the desktop card, the terminal, `kgraf
/// deps allow`): a worker that adds a dependency during a run has the host
/// fetch it, up to [`MAX_WORKER_FETCHES`] times per run. The names of the
/// crates a worker adds are sent to the registry.
pub fn worker_fetch_scope(registry: Registry) -> String {
    format!(
        "This consent also lets workers fetch crates they add during the run, from {registry}, \
         up to {MAX_WORKER_FETCHES} fetches per run; the names of those crates go to the registry."
    )
}

impl ConsentQuestion {
    pub fn text(&self) -> String {
        match &self.subject {
            ConsentSubject::Fetch(registry) => format!(
                "Fetch this project's dependencies from {registry}? They are not in the local \
                 cache, and workers build with the network off. kGraf would run `{}` on this \
                 machine, outside the workers' sandbox; it downloads and unpacks, and runs no \
                 build script. {}",
                registry.manual_command(),
                worker_fetch_scope(*registry)
            ),
            ConsentSubject::Install(installs) => install::question_text(installs),
        }
    }

    /// The command a person runs by hand instead, for "not now".
    pub fn manual_commands(&self) -> String {
        match &self.subject {
            ConsentSubject::Fetch(registry) => format!("`{}`", registry.manual_command()),
            ConsentSubject::Install(installs) => install_list(installs),
        }
    }

    /// The words `kgraf deps allow` takes for this question.
    pub fn allow_command(&self) -> String {
        match &self.subject {
            ConsentSubject::Fetch(_) => "kgraf deps allow".to_owned(),
            ConsentSubject::Install(installs) => {
                format!("kgraf deps allow {}", install_keys(installs))
            }
        }
    }
}

impl DepsError {
    /// The question to ask, when this is a missing answer.
    pub fn consent_question(&self) -> Option<ConsentQuestion> {
        match self {
            DepsError::ConsentNeeded {
                registry,
                project,
                detail,
            } => Some(ConsentQuestion {
                subject: ConsentSubject::Fetch(*registry),
                project: project.clone(),
                detail: detail.clone(),
            }),
            DepsError::InstallConsentNeeded { project, installs } => Some(ConsentQuestion {
                subject: ConsentSubject::Install(installs.clone()),
                project: project.clone(),
                detail: installs
                    .iter()
                    .map(|i| format!("{} ({}): {}", i.place(), i.ecosystem.name(), i.what))
                    .collect::<Vec<_>>()
                    .join("; "),
            }),
            _ => None,
        }
    }
}

/// What the gate did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    NotApplicable,
    Ready,
    Fetched(FetchReport),
    /// The check could not decide; the run goes on, and this is said.
    Unchecked(String),
}

/// Where the fetch is triggered from, for the diagnostics record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    RunStart,
    Worker,
}

impl Trigger {
    pub fn name(self) -> &'static str {
        match self {
            Trigger::RunStart => "run-start",
            Trigger::Worker => "worker",
        }
    }
}

/// What the consent rule says about a needed fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Fetch,
    AskFirst,
    Refuse { for_this_run: bool },
}

/// The consent rule mid-run: [`decide`], except that a refusal stored after
/// the run started wins over the run's own "allow" (a consent withdrawn
/// mid-run applies to the next build). A refusal stored before the start was
/// already overridden, knowingly, by that run's answer.
pub fn decide_mid_run(stored_now: Consent, stored_at_start: Consent, run: RunFetch) -> Decision {
    if stored_now == Consent::Refused && stored_at_start != Consent::Refused {
        return Decision::Refuse {
            for_this_run: false,
        };
    }
    decide(stored_now, run)
}

/// The consent rule, alone: a run's answer wins; otherwise the stored consent.
pub fn decide(stored: Consent, run: RunFetch) -> Decision {
    match (run, stored) {
        (RunFetch::Allow, _) => Decision::Fetch,
        (RunFetch::Refuse, _) => Decision::Refuse { for_this_run: true },
        (RunFetch::Stored, Consent::Allowed) => Decision::Fetch,
        (RunFetch::Stored, Consent::Refused) => Decision::Refuse {
            for_this_run: false,
        },
        (RunFetch::Stored, Consent::Unset) => Decision::AskFirst,
    }
}

/// The run-start gate: check; when missing, fetch only as [`decide`] allows,
/// then check again. The stored consent is read only when a fetch is needed,
/// so a project whose dependencies are ready never needs the file.
pub fn ensure(
    tool: &dyn DependencyTool,
    site: &FetchSite,
    store: &ConsentStore,
    run: RunFetch,
    trigger: Trigger,
) -> Result<GateOutcome, DepsError> {
    if !tool.applies(site) {
        if let Some(nested) = tool.nested_manifest(site) {
            let registry = tool.registry();
            return Ok(GateOutcome::Unchecked(format!(
                "the project has no manifest at its root, but {} exists; kGraf checks and \
                 fetches {registry} dependencies only for a root manifest, so a build in that \
                 folder can find a dependency missing (run `{}` there yourself)",
                nested.display(),
                registry.manual_command(),
            )));
        }
        return Ok(GateOutcome::NotApplicable);
    }
    let detail = match tool.check(site) {
        Readiness::NotApplicable => return Ok(GateOutcome::NotApplicable),
        Readiness::Ready => return Ok(GateOutcome::Ready),
        Readiness::Unchecked(why) => return Ok(GateOutcome::Unchecked(why)),
        Readiness::Missing(detail) => detail,
    };
    let registry = tool.registry();
    let project = site.source.clone();
    match decide(store.get(registry)?, run) {
        Decision::AskFirst => Err(DepsError::ConsentNeeded {
            registry,
            project,
            detail,
        }),
        Decision::Refuse { for_this_run } => Err(DepsError::Refused {
            registry,
            project,
            detail,
            for_this_run,
        }),
        Decision::Fetch => {
            fetch_and_confirm(tool, site, trigger, &Stop::default()).map(GateOutcome::Fetched)
        }
    }
}

/// Fetch, record the egress, and check again: a fetch that leaves a
/// dependency missing fails loud rather than letting workers find out.
pub fn fetch_and_confirm(
    tool: &dyn DependencyTool,
    site: &FetchSite,
    trigger: Trigger,
    stop: &Stop,
) -> Result<FetchReport, DepsError> {
    let registry = tool.registry();
    let result = tool.fetch(site, stop);
    record_fetch(registry, trigger, &result);
    let report = result?;
    match tool.check_watched(site, stop) {
        Readiness::Missing(detail) => Err(DepsError::StillMissing { registry, detail }),
        // A check that could not decide confirms nothing: after a fetch that
        // is a failure to confirm, said with its reason, never a success.
        Readiness::Unchecked(why) => Err(DepsError::StillMissing {
            registry,
            detail: format!("the check after the fetch could not confirm it: {why}"),
        }),
        Readiness::Ready | Readiness::NotApplicable => Ok(report),
    }
}

/// Every fetch is a network event with no tokens. It goes to the local
/// diagnostics file (decision 0069), and each caller also says it where a
/// person reads it: a notice at run start, the tool result for a worker.
fn record_fetch(registry: Registry, trigger: Trigger, result: &Result<FetchReport, DepsError>) {
    let (ok, downloaded) = match result {
        Ok(report) => (true, report.downloaded as u64),
        Err(_) => (false, 0),
    };
    kgraf_diag::record(kgraf_diag::DiagEvent::RegistryFetch {
        registry: registry.name().to_owned(),
        trigger: trigger.name().to_owned(),
        ok,
        downloaded,
    });
}

/// `text` with each of `paths` (as given and resolved) replaced by its
/// placeholder, longest first, then the user's home folder by `~`, then the
/// user-and-password part of any URL by `<redacted>`. For text a worker
/// reads: package manager errors name the host's cache, the mirror, the
/// user's folders and a proxy's credentials, none of which the worker needs.
pub(crate) fn redact(text: &str, paths: &[(&Path, &str)]) -> String {
    let mut pairs: Vec<(String, &str)> = Vec::new();
    for (path, placeholder) in paths {
        push_forms(&mut pairs, path, placeholder);
    }
    // The cargo home, when set (rustup sets it for cargo), has its own
    // placeholder, so one outside the home folder is replaced too.
    let cargo_home = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty());
    if let Some(cargo_home) = &cargo_home {
        push_forms(&mut pairs, Path::new(cargo_home), "<cargo-home>");
    }
    let home = home_dir();
    if let Some(home) = &home {
        push_forms(&mut pairs, home, "~");
    }
    // Longest first, so a path is replaced before a folder that holds it.
    pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
    let mut out = text.to_owned();
    for (from, to) in pairs {
        out = out.replace(&from, to);
    }
    redact_url_credentials(&out)
}

fn push_forms<'a>(pairs: &mut Vec<(String, &'a str)>, path: &Path, placeholder: &'a str) {
    let given = path.to_string_lossy().trim_end_matches('/').to_owned();
    // A bare "/" or an empty path would replace every slash.
    if given.len() > 1 {
        push_spellings(pairs, &given, placeholder, true);
    }
    if let Ok(resolved) = kgraf_core::plain_path::canonicalize(path) {
        let resolved = resolved.to_string_lossy().trim_end_matches('/').to_owned();
        if resolved.len() > 1 && resolved != given {
            push_spellings(pairs, &resolved, placeholder, false);
        }
    }
}

/// One path as a message may spell it: as written, and with `/` for each
/// backslash and without a `\\?\` prefix, which is how a `file://` URL names
/// a Windows path. `plain_first` keeps the order each caller pushed in.
fn push_spellings<'a>(
    pairs: &mut Vec<(String, &'a str)>,
    spelled: &str,
    placeholder: &'a str,
    plain_first: bool,
) {
    let mut forms = vec![spelled.to_owned()];
    let slashed = spelled.replace('\\', "/");
    if slashed != spelled {
        if let Some(bare) = slashed.strip_prefix("//?/") {
            forms.push(bare.to_owned());
        }
        forms.push(slashed);
    }
    for form in forms {
        if plain_first {
            pairs.push((form.clone(), placeholder));
            push_encoded(pairs, &form, placeholder);
        } else {
            push_encoded(pairs, &form, placeholder);
            pairs.push((form, placeholder));
        }
    }
}

/// The percent-encoded forms of `path` (a `file://` URL in a message spells a
/// space `%20`): every byte outside the unreserved set encoded, or every
/// byte outside a URL path's characters (which keeps `(`, `,` and the other
/// sub-delimiters), each with upper- and lowercase hex digits. Only when
/// encoding changes the text.
fn push_encoded<'a>(pairs: &mut Vec<(String, &'a str)>, path: &str, placeholder: &'a str) {
    const SUB_DELIMS: &[u8] = b"!$&'()*+,;=:@";
    for (upper, keep_sub_delims) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut out = String::with_capacity(path.len());
        for b in path.bytes() {
            let plain = b.is_ascii_alphanumeric()
                || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~')
                || (keep_sub_delims && SUB_DELIMS.contains(&b));
            if plain {
                out.push(b as char);
            } else if upper {
                out.push_str(&format!("%{b:02X}"));
            } else {
                out.push_str(&format!("%{b:02x}"));
            }
        }
        if out != path && !pairs.iter().any(|(f, _)| *f == out) {
            pairs.push((out, placeholder));
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `scheme://user:pass@host` becomes `scheme://<redacted>@host`.
fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        // The token runs to whitespace or a quote. A password may hold `/`,
        // `)` or `?`, so the credentials end at the last `@` of the token
        // (the host follows it). Over-redacting a path with an `@` in it is
        // the safe side.
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '<' | '>'))
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(i) => {
                out.push_str("<redacted>");
                out.push_str(&authority[i..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// A refetcher for tests that assemble environments by hand: no package
/// manager applies and nothing is allowed, so it never fetches.
#[cfg(test)]
pub(crate) fn inert_refetch() -> std::sync::Arc<dyn kgraf_tools::DependencyRefetch> {
    std::sync::Arc::new(HostRefetch::new(
        std::sync::Arc::new(tests::FakeTool::default()),
        ConsentStore::in_state_dir(Path::new("/nonexistent/kgraf-test-state")),
        RunFetch::Stored,
        PathBuf::from("/nonexistent/kgraf-test-checkout"),
    ))
}

#[cfg(test)]
pub(crate) mod tests;
