//! kGraf runs a project's dependency install itself, with the user's consent,
//! in its sandbox (decision 0087, amended 2026-10-07).
//!
//! ```text
//!   run start --> missing installs (installs.rs) --none--> go on
//!                    | each install, with its exact command
//!                    v
//!               this run's answer (--fetch-deps / --no-fetch-deps, "Allow
//!               once") over the stored one for that command
//!                    |-- unset (any) --> fail loud: InstallConsentNeeded,
//!                    |                   before anything runs (a caller that
//!                    |                   can ask asks, then starts again)
//!                    |-- refused ------> go on, with the not-installed notice
//!                    `-- allowed ------> run it in the sandbox --> check again
//!                                           |-- fails -----> fail loud: output tail,
//!                                           |                blocked hosts
//!                                           `-- still missing --> fail loud
//! ```
//!
//! **The sandbox an install runs in.** Writes: the checkout (the install's
//! own tree: `node_modules`, `.venv`, `vendor`, `obj`) and the project's
//! package stores (`kgraf_sandbox::PackageStores`: download caches, and the
//! stores that Go, Maven, Gradle and NuGet install into). The checkout's
//! `.git` stays read-only, and its state directory `.kgraf` (consents,
//! permission rules, transcripts) is hidden: neither readable nor writable,
//! apart from the stores inside it. Network (macOS, Linux): only kGraf's
//! egress proxy (`kgraf_sandbox::egress`), which reaches only the hosts
//! allowed for the ecosystem ([`InstallHosts`]). Install scripts (npm
//! lifecycle scripts, a `setup.py` build) run inside the same sandbox.
//! Windows cannot limit the network (decision 0011): the install runs with
//! it open, still with writes confined, and says so.
//!
//! **No credentials.** The install's home folder is an empty folder in the
//! stores, so no package manager finds the user's `~/.npmrc`, `~/.pypirc`,
//! `~/.netrc`, Maven `settings.xml` or other login files; pip reads no
//! configuration file at all; credential-shaped variables are removed by
//! the sandbox. A project that needs a private registry fails loud, naming
//! the blocked host and what to configure.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kgraf_sandbox::egress::HostRule;

use super::{
    Consent, ConsentStore, DepsError, Ecosystem, InstallEnv, MissingInstall, RunFetch, Stop,
    missing_installs,
};

/// The longest an install may run before it is killed.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How much of an install's output a failure shows, from its end.
const TAIL_CHARS: usize = 4_000;

/// Most output kept while an install runs.
const KEPT_BYTES: usize = 256 * 1024;

/// `settings.toml` key: the hosts each ecosystem's install may reach, as a
/// table of lists (`[install_registries] npm = ["registry.npmjs.org"]`). An
/// ecosystem the table names gets exactly its list; one it leaves out gets
/// [`InstallHosts::default_hosts`].
pub const KEY_INSTALL_REGISTRIES: &str = "install_registries";

/// The hosts each ecosystem's install may reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallHosts {
    lists: BTreeMap<Ecosystem, Vec<HostRule>>,
}

impl Default for InstallHosts {
    fn default() -> Self {
        Self::defaults()
    }
}

impl InstallHosts {
    /// Each ecosystem's public registry hosts. Where a registry hands its
    /// downloads to another host, that host is listed too (measured
    /// 2026-10-07 with `curl -I`): Gradle's plugin portal sends jars to
    /// `plugins-artifacts.gradle.org`, and its distribution service sends the
    /// wrapper's download to a GitHub release
    /// (`release-assets.githubusercontent.com`); Packagist's packages
    /// download from `api.github.com`, which sends them on to
    /// `codeload.github.com`.
    pub fn default_hosts(ecosystem: Ecosystem) -> &'static [&'static str] {
        match ecosystem {
            Ecosystem::Npm => &["registry.npmjs.org", "registry.yarnpkg.com"],
            Ecosystem::Python => &["pypi.org", "files.pythonhosted.org"],
            Ecosystem::Go => &["proxy.golang.org", "sum.golang.org"],
            Ecosystem::Maven => &["repo.maven.apache.org"],
            Ecosystem::Gradle => &[
                "plugins.gradle.org",
                "plugins-artifacts.gradle.org",
                "services.gradle.org",
                "repo.maven.apache.org",
                "github.com",
                "release-assets.githubusercontent.com",
            ],
            Ecosystem::Dotnet => &["api.nuget.org"],
            Ecosystem::Composer => &[
                "repo.packagist.org",
                "api.github.com",
                "codeload.github.com",
            ],
            Ecosystem::Bundler => &["rubygems.org", "index.rubygems.org"],
        }
    }

    /// The built-in lists.
    pub fn defaults() -> Self {
        let lists = Ecosystem::ALL
            .iter()
            .map(|e| {
                let rules = Self::default_hosts(*e)
                    .iter()
                    .filter_map(|h| HostRule::parse(h).ok())
                    .collect();
                (*e, rules)
            })
            .collect();
        Self { lists }
    }

    /// The hosts `ecosystem`'s install may reach.
    pub fn for_ecosystem(&self, ecosystem: Ecosystem) -> &[HostRule] {
        self.lists.get(&ecosystem).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Replace `ecosystem`'s list.
    pub fn set(&mut self, ecosystem: Ecosystem, rules: Vec<HostRule>) {
        self.lists.insert(ecosystem, rules);
    }

    /// Whether `ecosystem`'s list is the built-in one.
    pub fn is_default(&self, ecosystem: Ecosystem) -> bool {
        let default: Vec<HostRule> = Self::default_hosts(ecosystem)
            .iter()
            .filter_map(|h| HostRule::parse(h).ok())
            .collect();
        self.for_ecosystem(ecosystem) == default.as_slice()
    }

    /// Parse each of `entries` for `ecosystem`, naming the first bad one.
    pub fn parse_list(entries: &[String]) -> Result<Vec<HostRule>, String> {
        entries
            .iter()
            .filter(|e| !e.trim().is_empty())
            .map(|e| HostRule::parse(e))
            .collect()
    }

    /// The `install_registries` table of a settings file's text. Other keys
    /// are not read. An unknown ecosystem, a value that is not a list of
    /// strings, or an entry [`HostRule::parse`] refuses is an error naming it.
    pub fn from_toml_str(text: &str) -> Result<Self, String> {
        let table: toml::Table = text
            .parse()
            .map_err(|e| format!("settings file is not TOML: {e}"))?;
        let mut hosts = Self::defaults();
        let Some(value) = table.get(KEY_INSTALL_REGISTRIES) else {
            return Ok(hosts);
        };
        let Some(lists) = value.as_table() else {
            return Err(format!(
                "settings: `{KEY_INSTALL_REGISTRIES}` must be a table"
            ));
        };
        for (key, list) in lists {
            let ecosystem = Ecosystem::ALL
                .iter()
                .copied()
                .find(|e| e.key() == key)
                .ok_or_else(|| {
                    format!(
                        "settings: `{KEY_INSTALL_REGISTRIES}.{key}` names no ecosystem (one of {})",
                        Ecosystem::ALL.map(Ecosystem::key).join(", ")
                    )
                })?;
            let entries: Vec<String> = list
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| {
                    format!(
                        "settings: `{KEY_INSTALL_REGISTRIES}.{key}` must be a list of host names"
                    )
                })?;
            let rules = Self::parse_list(&entries)
                .map_err(|e| format!("settings: `{KEY_INSTALL_REGISTRIES}.{key}`: {e}"))?;
            hosts.set(ecosystem, rules);
        }
        Ok(hosts)
    }

    /// Reads `path`; a missing file is the built-in lists.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::defaults()),
            Err(e) => Err(format!("cannot read settings file {}: {e}", path.display())),
        }
    }

    /// The user's lists, from the same settings file as the run limits
    /// (`crate::limits::UserLimits::default_path`).
    pub fn load_default() -> Result<Self, String> {
        match crate::limits::UserLimits::default_path() {
            Some(p) => Self::load(&p),
            None => Ok(Self::defaults()),
        }
    }
}

/// What one install did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallReport {
    pub install: MissingInstall,
    pub elapsed: Duration,
    /// `host:port` of each host the proxy let it reach.
    pub contacted: Vec<String>,
    /// `host:port` of each host the proxy refused it.
    pub blocked: Vec<String>,
    /// Windows: the network was open (the sandbox cannot limit it).
    pub network_open: bool,
}

impl InstallReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "ran `{}` in {} in kGraf's sandbox in {:.1} s",
            self.install.command,
            self.install.place(),
            self.elapsed.as_secs_f64()
        );
        if self.network_open {
            s.push_str(&format!("; {WINDOWS_NETWORK_OPEN}"));
        } else if self.contacted.is_empty() {
            s.push_str("; no host contacted");
        } else {
            s.push_str(&format!("; reached {}", self.contacted.join(", ")));
        }
        if !self.blocked.is_empty() {
            s.push_str(&format!(
                "; blocked {} (not needed for the install to succeed)",
                self.blocked.join(", ")
            ));
        }
        s
    }
}

/// How an install reaches the network on a sandbox backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallNetwork {
    /// Only kGraf's egress proxy, which allows only the registry hosts
    /// (macOS Seatbelt; Linux with a network namespace).
    ThroughProxy,
    /// The network open, with this notice: the backend cannot limit it
    /// (Windows, decision 0011). Writes stay confined.
    OpenWithNotice(&'static str),
}

/// The network rule for an install on `backend`: open with a notice only
/// where the backend cannot deny the network, through the proxy everywhere
/// else (a Linux kernel without user namespaces refuses the proxy policy
/// when the command is built, and the install fails loud there).
pub fn install_network(backend: kgraf_sandbox::Backend) -> InstallNetwork {
    match backend {
        kgraf_sandbox::Backend::WindowsRestrictedToken => {
            InstallNetwork::OpenWithNotice(WINDOWS_NETWORK_OPEN)
        }
        kgraf_sandbox::Backend::MacosSeatbelt
        | kgraf_sandbox::Backend::LinuxLandlock
        | kgraf_sandbox::Backend::WindowsSandboxUser => InstallNetwork::ThroughProxy,
    }
}

/// [`install_network`] for this platform's backend.
fn this_install_network() -> InstallNetwork {
    match kgraf_sandbox::platform_isolation() {
        Ok(strength) => install_network(strength.backend),
        // No backend: the command is refused when it is built, loud.
        Err(_) => InstallNetwork::ThroughProxy,
    }
}

/// What a Windows install says: decision 0011, no network limit there.
pub const WINDOWS_NETWORK_OPEN: &str = "network not limited: the Windows sandbox cannot limit \
    network access without administrator rights (decision 0011), so the install ran with the \
    network open, and its reads were not confined either; its writes were still confined; \
    run `kgraf sandbox setup` once, with administrator rights, to block it (decision 0097)";

/// Runs one install. [`SandboxedInstall`] is the real one; tests pass a fake.
pub trait InstallRunner: Send + Sync {
    fn run(
        &self,
        install: &MissingInstall,
        checkout: &Path,
        hosts: &[HostRule],
        stop: &Stop,
    ) -> Result<InstallReport, DepsError>;
}

/// What the install gate did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallOutcome {
    pub installed: Vec<InstallReport>,
    /// Installs left missing on purpose: refused for the project or the run.
    pub declined: Vec<MissingInstall>,
    /// Whether `declined` was this run's answer (`--no-fetch-deps`).
    pub declined_for_this_run: bool,
}

/// The run-start install gate: every missing install, decided by this run's
/// answer over the stored one for its exact command; with any unanswered,
/// nothing runs and the question goes back to the caller.
#[allow(clippy::too_many_arguments)]
pub fn ensure_installs(
    checkout: &Path,
    env: &InstallEnv,
    python_ready: &dyn Fn() -> bool,
    store: &ConsentStore,
    run: RunFetch,
    runner: &dyn InstallRunner,
    hosts: &dyn Fn() -> Result<InstallHosts, String>,
    stop: &Stop,
) -> Result<InstallOutcome, DepsError> {
    let missing = missing_installs(checkout, env, python_ready);
    let mut outcome = InstallOutcome::default();
    if missing.is_empty() {
        return Ok(outcome);
    }
    let mut ask = Vec::new();
    let mut allowed = Vec::new();
    for m in missing {
        let answer = match run {
            RunFetch::Allow => Consent::Allowed,
            RunFetch::Refuse => {
                outcome.declined_for_this_run = true;
                Consent::Refused
            }
            RunFetch::Stored => store.install_consent(&m)?,
        };
        match answer {
            Consent::Allowed => allowed.push(m),
            Consent::Refused => outcome.declined.push(m),
            Consent::Unset => ask.push(m),
        }
    }
    if !ask.is_empty() {
        return Err(DepsError::InstallConsentNeeded {
            project: checkout.to_path_buf(),
            installs: ask,
        });
    }
    // Read only when something will run: the settings file need not exist,
    // or even be readable, for a run with nothing to install.
    let hosts = if allowed.is_empty() {
        InstallHosts::defaults()
    } else {
        hosts().map_err(|detail| DepsError::Settings { detail })?
    };
    for m in &allowed {
        let result = runner.run(m, checkout, hosts.for_ecosystem(m.ecosystem), stop);
        kgraf_diag::record(kgraf_diag::DiagEvent::RegistryFetch {
            registry: m.ecosystem.key().to_owned(),
            trigger: "install".to_owned(),
            ok: result.is_ok(),
            downloaded: 0,
        });
        outcome.installed.push(result?);
    }
    if !allowed.is_empty() {
        let env = InstallEnv {
            stores: kgraf_sandbox::PackageStores::find(checkout),
            ..env.clone()
        };
        let after = missing_installs(checkout, &env, python_ready);
        for m in &allowed {
            if let Some(still) = after
                .iter()
                .find(|a| a.ecosystem == m.ecosystem && a.dir == m.dir)
            {
                return Err(DepsError::InstallIncomplete {
                    install: m.clone(),
                    what: still.what.clone(),
                });
            }
        }
    }
    Ok(outcome)
}

/// The question for `installs`, for the terminal and the desktop card.
pub fn question_text(installs: &[MissingInstall]) -> String {
    question_text_for(installs, this_install_network())
}

/// [`question_text`] for a given network rule, so each platform's words are
/// tested on every platform.
pub fn question_text_for(installs: &[MissingInstall], rule: InstallNetwork) -> String {
    let commands = installs
        .iter()
        .map(|i| format!("`{}` in {} ({})", i.command, i.place(), i.ecosystem.name()))
        .collect::<Vec<_>>()
        .join("; ");
    let network = match rule {
        // Decision 0011: the Windows backend confines writes only. Its reads
        // are open, and so is the network: what an install script reads, it
        // can send anywhere.
        InstallNetwork::OpenWithNotice(_) => {
            "On Windows the sandbox confines neither reads nor the network (decision 0011): \
             the install runs with the network open, and it and its scripts can read any file \
             you can, your credential files included, and send it anywhere. kGraf gives the install an empty home folder, so \
             package managers do not use your logins by themselves; a script can still read \
             them."
        }
        InstallNetwork::ThroughProxy => {
            "The install reaches only the package registries allowed for it in Settings, \
             through kGraf's proxy. Your credential files are not readable to it, so a \
             registry that needs a login is not reachable."
        }
    };
    format!(
        "Run this project's dependency install? Workers run with the network off, and these \
         installs are missing. kGraf would run, in its sandbox: {commands}. The install and its \
         scripts write only inside the project (never its .git or .kgraf) and kGraf's package \
         cache. {network} If the command changes, kGraf asks again; a changed lockfile or \
         package script does not count, and runs in the sandbox like the rest."
    )
}

/// The real install: the command in kGraf's sandbox, through the egress
/// proxy on macOS and Linux.
#[derive(Clone, Debug)]
pub struct SandboxedInstall {
    pub timeout: Duration,
}

impl Default for SandboxedInstall {
    fn default() -> Self {
        Self {
            timeout: INSTALL_TIMEOUT,
        }
    }
}

/// Toolchain managers whose home is found from `HOME` by default: with the
/// install's own home, each is pointed at the user's real folder, when that
/// exists and the variable is not set, so a shim (`pyenv`, `volta`) still
/// finds its toolchains. None of these folders holds a registry login.
const TOOLCHAIN_HOMES: &[(&str, &str)] = &[
    ("CARGO_HOME", ".cargo"),
    ("RUSTUP_HOME", ".rustup"),
    ("PYENV_ROOT", ".pyenv"),
    ("RBENV_ROOT", ".rbenv"),
    ("ASDF_DATA_DIR", ".asdf"),
    ("MISE_DATA_DIR", ".local/share/mise"),
    ("VOLTA_HOME", ".volta"),
    ("NVM_DIR", ".nvm"),
    ("SDKMAN_DIR", ".sdkman"),
    ("COREPACK_HOME", ".cache/node/corepack"),
];

/// Prefixes of variables through which a package manager, or git under it,
/// takes configuration that can hold a login or send it elsewhere, matched
/// without regard to case: pip's and uv's index URLs (which can carry a
/// password), Poetry's and PDM's repositories, npm's, Yarn's, pnpm's and
/// Bun's settings (a registry and its token), Bundler's per-host
/// credentials (`BUNDLE_GEMS__EXAMPLE__COM=user:pass`), Composer's
/// settings, NuGet's (`VSS_NUGET_EXTERNAL_FEED_ENDPOINTS` holds tokens),
/// Gradle project properties (repository logins), git's config file
/// locations (the sandbox removes git's environment entries itself, see
/// `inherited_configuration`), and cargo's registries.
const CONFIGURATION_PREFIXES: &[&str] = &[
    "PIP_",
    "UV_",
    "POETRY_",
    "PDM_",
    "PIPENV_",
    "TWINE_",
    "NPM_CONFIG_",
    "YARN_",
    "PNPM_",
    "BUN_",
    "BUNDLE_",
    "COMPOSER_",
    "NUGET_",
    "VSS_NUGET_",
    "ORG_GRADLE_PROJECT_",
    "GIT_CONFIG",
    "CARGO_REGISTRIES_",
    "CARGO_REGISTRY_",
];

/// Single variables of the same kind: Go's proxy and private-module
/// settings (a proxy URL can carry a login), Gradle's and the JVM's option
/// variables (system properties with passwords, or a proxy of the user's),
/// Maven's arguments, the programs git and ssh ask for a password, and the
/// netrc file curl and Go read logins from.
const CONFIGURATION_NAMES: &[&str] = &[
    "GOPROXY",
    "GONOPROXY",
    "GOPRIVATE",
    "GONOSUMDB",
    "GONOSUMCHECK",
    "GOINSECURE",
    "GOFLAGS",
    "GOAUTH",
    "GRADLE_OPTS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "MAVEN_ARGS",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "NETRC",
];

/// Which of `inherited` (the variable names kGraf's own process has) an
/// install must not inherit: package manager configuration
/// ([`CONFIGURATION_PREFIXES`], [`CONFIGURATION_NAMES`]). The sandbox removes
/// credential-shaped names already; these names do not look like
/// credentials, yet their values can be one (`PIP_INDEX_URL=https://user:
/// token@...`), and the package manager reads them as it would read its
/// configuration file. Removed before kGraf sets its own values for some of
/// them ([`install_env`]).
pub(crate) fn inherited_configuration(
    inherited: impl Iterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    inherited
        .filter(|name| {
            let Some(name) = name.to_str() else {
                return false;
            };
            let upper = name.to_ascii_uppercase();
            // Where pnpm and Bun keep their own programs, not configuration.
            if matches!(upper.as_str(), "PNPM_HOME" | "BUN_INSTALL") {
                return false;
            }
            // The sandbox already removes the host's git configuration
            // entries and sets its own under the same names; removing the
            // names here would remove the sandbox's entries too.
            if kgraf_sandbox::is_git_config_entry_var(name) {
                return false;
            }
            CONFIGURATION_PREFIXES.iter().any(|p| upper.starts_with(p))
                || CONFIGURATION_NAMES.contains(&upper.as_str())
        })
        .collect()
}

/// The variables of an install, apart from the proxy's: its own home, the
/// download caches and stores in the project's stores, no configuration file
/// that could hold a login.
pub(crate) fn install_env(
    stores: &Path,
    home: &Path,
    tmp: &Path,
    real_home: Option<&Path>,
    java_proxy: Option<&str>,
) -> Vec<(String, std::ffi::OsString)> {
    use kgraf_sandbox::store_layout as layout;
    let mut env: Vec<(String, std::ffi::OsString)> = Vec::new();
    let mut set = |k: &str, v: std::ffi::OsString| env.push((k.to_owned(), v));
    let p = |rel: &str| stores.join(rel).into_os_string();
    set("HOME", home.as_os_str().to_owned());
    if cfg!(windows) {
        set("USERPROFILE", home.as_os_str().to_owned());
    }
    set("XDG_CONFIG_HOME", home.join(".config").into_os_string());
    set("XDG_CACHE_HOME", home.join(".cache").into_os_string());
    set("XDG_DATA_HOME", home.join(".local/share").into_os_string());
    // npm (and pnpm, which reads npm's configuration): no user or global
    // configuration file, the cache in the stores, no audit or update
    // check (each a further request).
    set(
        "NPM_CONFIG_USERCONFIG",
        tmp.join("user-npmrc").into_os_string(),
    );
    set(
        "NPM_CONFIG_GLOBALCONFIG",
        tmp.join("global-npmrc").into_os_string(),
    );
    set("NPM_CONFIG_CACHE", p("npm-cache"));
    // pnpm's store goes to its default under the install's own home (in the
    // stores); an `npm_config_store_dir` would make npm warn on every run.
    set("npm_config_audit", "false".into());
    set("npm_config_fund", "false".into());
    set("npm_config_update_notifier", "false".into());
    set("YARN_CACHE_FOLDER", p("yarn-cache"));
    set("BUN_INSTALL_CACHE_DIR", p("bun-cache"));
    // pip: no configuration file at all (a site `pip.conf` can name an index
    // with its password); the cache in the stores.
    set(
        "PIP_CONFIG_FILE",
        if cfg!(windows) { "nul" } else { "/dev/null" }.into(),
    );
    set("PIP_CACHE_DIR", p("pip-cache"));
    set("PIP_DISABLE_PIP_VERSION_CHECK", "1".into());
    set("GOMODCACHE", p(layout::GO));
    set("GOPATH", home.join("go").into_os_string());
    set("GOCACHE", p("go-build"));
    // The JVM takes its home from the password database, not `HOME`: Maven
    // would read the user's `settings.xml` (repository passwords) without
    // `user.home`.
    // Both options are split on white space by the JVM and the `mvn` script:
    // a path with a space goes as a link without one (`jvm_path`).
    let user_home = format!("-Duser.home={}", kgraf_sandbox::jvm_path(home).display());
    let mut maven = std::ffi::OsString::from("-Dmaven.repo.local=");
    maven.push(kgraf_sandbox::jvm_path(&stores.join(layout::MAVEN)));
    maven.push(format!(" {user_home}"));
    set("MAVEN_OPTS", maven);
    set(
        "JAVA_TOOL_OPTIONS",
        match java_proxy {
            Some(proxy) => format!("{proxy} {user_home}"),
            None => user_home.clone(),
        }
        .into(),
    );
    set("GRADLE_USER_HOME", p(layout::GRADLE_HOME));
    // No build daemon outlives the install: Gradle's daemon and MSBuild's
    // reused nodes detach from the install's process group and would go on
    // running in the sandbox, with the checkout writable, after kGraf took
    // the install as done.
    set("GRADLE_OPTS", "-Dorg.gradle.daemon=false".into());
    set("MSBUILDDISABLENODEREUSE", "1".into());
    set("NUGET_PACKAGES", p(layout::NUGET));
    set("DOTNET_CLI_HOME", home.as_os_str().to_owned());
    set("COMPOSER_HOME", p("composer"));
    set("COMPOSER_CACHE_DIR", p("composer-cache"));
    set("BUNDLE_USER_HOME", p("bundler"));
    if let Some(real) = real_home {
        for (var, rel) in TOOLCHAIN_HOMES {
            let dir = real.join(rel);
            if std::env::var_os(var).is_none() && dir.is_dir() {
                set(var, dir.into_os_string());
            }
        }
    }
    env
}

impl InstallRunner for SandboxedInstall {
    fn run(
        &self,
        install: &MissingInstall,
        checkout: &Path,
        hosts: &[HostRule],
        stop: &Stop,
    ) -> Result<InstallReport, DepsError> {
        let failed = |detail: String| DepsError::InstallFailed {
            install: install.clone(),
            detail,
        };
        let checkout = kgraf_core::plain_path::canonicalize(checkout)
            .map_err(|e| DepsError::io(format!("cannot resolve {}", checkout.display()), e))?;
        let dir = kgraf_core::plain_path::canonicalize(checkout.join(&install.dir))
            .map_err(|e| DepsError::io(format!("cannot resolve {}", install.dir.display()), e))?;
        if !dir.starts_with(&checkout) {
            return Err(failed(format!(
                "{} resolves outside the project; nothing was run",
                install.dir.display()
            )));
        }
        let state = checkout.join(kgraf_core::legacy::STATE_DIR);
        let stores = kgraf_sandbox::package_stores_root(&checkout);
        let home = stores.join("home");
        let tmp = stores.join("tmp");
        if tmp.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        std::fs::create_dir_all(&stores)
            .map_err(|e| DepsError::io(format!("cannot create {}", stores.display()), e))?;
        // Inside the stores an earlier install could write anything, a link
        // included: every host-side write there refuses a link (decision
        // 0083). The install's own store exists before it runs: Maven is
        // given it through a link when its path has a space (`jvm_path`), and
        // a JVM cannot make a folder through a link that points at nothing.
        let mut made = vec![
            "home/.kgraf-home".to_owned(),
            // npm reads these as its user and global configuration: empty.
            "tmp/user-npmrc".to_owned(),
            "tmp/global-npmrc".to_owned(),
        ];
        if let Some(rel) = store_of(install.ecosystem) {
            made.push(format!("{rel}/.kgraf-store"));
        }
        for rel in &made {
            host_write(&stores, rel, b"").map_err(|e| {
                DepsError::io(format!("cannot prepare {} in the package stores", rel), e)
            })?;
        }
        let real_home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);

        #[cfg(unix)]
        let proxy = kgraf_sandbox::egress::EgressProxy::start(hosts.to_vec())
            .map_err(|e| failed(format!("kGraf's egress proxy did not start: {e}")))?;
        #[cfg(unix)]
        let network = proxy.network();
        #[cfg(not(unix))]
        let network = {
            let _ = hosts;
            kgraf_sandbox::Network::Allow
        };
        let rule = this_install_network();
        // Every process of the install holds a shared lock on this file until
        // it exits, so one that leaves the install's process group is still
        // seen ([`wait_bounded`]). In the state directory, which the install
        // can neither read nor write, so it cannot replace the file.
        #[cfg(unix)]
        let hold = HoldFile::create(&state)
            .map_err(|e| DepsError::io("cannot make the install's hold file", e))?;
        let policy =
            kgraf_sandbox::SandboxPolicy::new(vec![checkout.clone(), stores.clone()], network)
                .hide(state)
                .with_tmp_dir(tmp.clone())
                // Windows is partial by its nature (reads and the network); the
                // install says so. Elsewhere a partial backend is refused.
                .accept_partial_isolation(matches!(rule, InstallNetwork::OpenWithNotice(_)));
        #[cfg(unix)]
        let policy = policy.holding(hold.path.clone());
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("cmd", vec!["/D", "/S", "/C", install.command.as_str()])
        } else {
            ("/bin/sh", vec!["-c", install.command.as_str()])
        };
        let mut cmd = kgraf_sandbox::sandboxed_command(&policy, program, &dir)
            .map_err(|e| failed(format!("the sandbox refused it: {e}")))?;
        cmd.args(&args)
            .stdin(kgraf_sandbox::Redirect::Null)
            .stdout(kgraf_sandbox::Redirect::Piped)
            .stderr(kgraf_sandbox::Redirect::Piped);
        // Before kGraf's own values below, which set some of the same names.
        for name in inherited_configuration(std::env::vars_os().map(|(k, _)| k)) {
            cmd.env_remove(name);
        }
        #[cfg(unix)]
        let java_proxy = {
            let mut java = None;
            for (k, v) in proxy.env() {
                if k == "JAVA_TOOL_OPTIONS" {
                    java = Some(v);
                } else {
                    cmd.env(k, v);
                }
            }
            for k in kgraf_sandbox::egress::EgressProxy::REMOVED_ENV {
                cmd.env_remove(k);
            }
            java
        };
        #[cfg(not(unix))]
        let java_proxy: Option<String> = None;
        for (k, v) in install_env(
            &stores,
            &home,
            &tmp,
            real_home.as_deref(),
            java_proxy.as_deref(),
        ) {
            cmd.env(k, v);
        }
        let started = Instant::now();
        #[cfg(unix)]
        let spawned = std::time::SystemTime::now();
        #[cfg(unix)]
        let child = proxy
            .spawn(&mut cmd)
            .map_err(|e| failed(format!("it did not start: {e}")))?;
        #[cfg(not(unix))]
        let child = cmd
            .spawn()
            .map_err(|e| failed(format!("it did not start: {e}")))?;
        #[cfg(unix)]
        let (status, tail) = wait_bounded(child, self.timeout, stop, &hold.path, spawned);
        #[cfg(not(unix))]
        let (status, tail) = wait_bounded(child, self.timeout, stop);
        let elapsed = started.elapsed();
        #[cfg(unix)]
        let (contacted, blocked) = (proxy.contacted(), proxy.blocked());
        #[cfg(not(unix))]
        let (contacted, blocked): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        let ended = match status {
            Ok(true) => None,
            Ok(false) => Some("it exited with a failure.".to_owned()),
            Err(why) => Some(format!("{why}.")),
        };
        if let Some(ended) = ended {
            let mut detail = ended;
            if !blocked.is_empty() {
                detail.push_str(&blocked_hosts_text(install.ecosystem, &blocked));
            }
            if let InstallNetwork::OpenWithNotice(notice) = rule {
                detail.push_str(&format!(" ({notice}.)"));
            }
            detail.push_str(&format!(" Its output ends:\n{}", tail_of(&tail)));
            return Err(failed(detail));
        }
        // Only now do later commands read the store instead of the user's.
        if let Some(rel) = store_of(install.ecosystem) {
            kgraf_sandbox::mark_store_complete(&checkout, rel)
                .map_err(|e| DepsError::io("cannot mark the package store as filled", e))?;
        }
        Ok(InstallReport {
            install: install.clone(),
            elapsed,
            contacted,
            blocked,
            network_open: matches!(rule, InstallNetwork::OpenWithNotice(_)),
        })
    }
}

/// Write the file `rel` beneath `stores` from the host, making its folders,
/// never through a link or (on Windows) a junction
/// (`kgraf_sandbox::write_confined`).
fn host_write(stores: &Path, rel: &str, content: &[u8]) -> std::io::Result<()> {
    kgraf_sandbox::write_confined(stores, Path::new(rel), content)
}

/// The package store an ecosystem installs into, for those that install
/// into a store rather than the project (`kgraf_sandbox::PackageStores`).
fn store_of(ecosystem: Ecosystem) -> Option<&'static str> {
    use kgraf_sandbox::store_layout as layout;
    match ecosystem {
        Ecosystem::Go => Some(layout::GO),
        Ecosystem::Maven => Some(layout::MAVEN),
        Ecosystem::Gradle => Some(layout::GRADLE_HOME),
        Ecosystem::Dotnet => Some(layout::NUGET),
        Ecosystem::Npm | Ecosystem::Python | Ecosystem::Composer | Ecosystem::Bundler => None,
    }
}

/// What a failure with blocked hosts says: each host, and the two ways to
/// allow it.
pub fn blocked_hosts_text(ecosystem: Ecosystem, blocked: &[String]) -> String {
    format!(
        " kGraf blocked {}: not an allowed host for {} installs. If the install needs it, allow \
         it in Settings (install registries) or the settings file \
         (`[{KEY_INSTALL_REGISTRIES}] {} = [...]`, keeping the hosts you still need), then start \
         again. A private registry that needs a login cannot be used by kGraf's install: kGraf \
         does not read your credential files; run the install yourself.",
        blocked.join(", "),
        ecosystem.name(),
        ecosystem.key()
    )
}

/// The last [`TAIL_CHARS`] characters of `text`, from a line start.
fn tail_of(text: &str) -> String {
    let t = text.trim_end();
    if t.chars().count() <= TAIL_CHARS {
        return t.to_owned();
    }
    let start = t
        .char_indices()
        .rev()
        .nth(TAIL_CHARS - 1)
        .map_or(0, |(i, _)| i);
    let rest = &t[start..];
    match rest.find('\n') {
        Some(nl) => format!("...\n{}", &rest[nl + 1..]),
        None => format!("...{rest}"),
    }
}

/// Read `streams` on threads into one buffer, line by line, keeping at most
/// [`KEPT_BYTES`] (the newest).
fn collect_output(
    streams: Vec<Box<dyn std::io::Read + Send>>,
) -> (Arc<Mutex<String>>, Vec<std::thread::JoinHandle<()>>) {
    let output = Arc::new(Mutex::new(String::new()));
    let readers = streams
        .into_iter()
        .map(|stream| {
            let output = output.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                    let mut out = output.lock().unwrap_or_else(|p| p.into_inner());
                    out.push_str(&String::from_utf8_lossy(&line));
                    if out.len() > KEPT_BYTES {
                        let cut = out.len() - KEPT_BYTES / 2;
                        let cut = (cut..out.len())
                            .find(|i| out.is_char_boundary(*i))
                            .unwrap_or(0);
                        out.replace_range(..cut, "");
                    }
                    line.clear();
                }
            })
        })
        .collect();
    (output, readers)
}

/// The output so far, once the readers end or `wait` passes: a reader is
/// left behind only when a process outside the killed group holds a pipe.
fn output_of(
    output: &Arc<Mutex<String>>,
    readers: Vec<std::thread::JoinHandle<()>>,
    wait: Duration,
) -> String {
    let deadline = Instant::now() + wait;
    for r in readers {
        while !r.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if r.is_finished() {
            let _ = r.join();
        }
    }
    output.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// The install's hold file (`SandboxPolicy::holding`), removed when the
/// install is done.
#[cfg(unix)]
struct HoldFile {
    path: PathBuf,
}

#[cfg(unix)]
impl HoldFile {
    /// A new, empty hold file in `state`, named for this process and a
    /// counter so two installs never share one.
    fn create(state: &Path) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = state.join(format!(
            "install-hold-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self { path })
    }
}

#[cfg(unix)]
impl Drop for HoldFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// After the install's group is gone: if a process it started still holds
/// `hold`, it left the group (its own `setsid`) and would go on writing the
/// checkout. On macOS it is killed by name ([`stop_holders`]), as was every
/// one the watch found while the install ran ([`EscapeWatch`]); either way
/// the install fails. `None` when nothing was left behind. Linux needs no
/// names: the install is PID 1 of its own PID namespace, and the kernel ends
/// every process left in it when the install's first process exits.
///
/// What escapes this too: a process that closes the descriptors it inherited
/// and leaves the group (decision 0084's known gap on macOS); a build
/// daemon is kept from starting by [`install_env`].
#[cfg(unix)]
fn left_behind(hold: &Path, watched: Watched) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let found = stop_holders(
            hold,
            kgraf_sandbox::lifecycle::holders,
            kgraf_sandbox::lifecycle::GROUP_EXIT_GRACE,
            watched.killed,
        );
        match watched.failed {
            None => found,
            Some(e) => Some(format!(
                "{}kGraf could not watch for processes that leave the install's group while \
                 it ran ({e}), so one may have changed files in the project unseen. An install \
                 kGraf could not watch is not taken as done",
                found.map(|f| format!("{f}. ")).unwrap_or_default()
            )),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let Watched {} = watched;
        if kgraf_sandbox::lifecycle::hold_free(hold) {
            return None;
        }
        Some(STILL_RUNNING.to_owned())
    }
}

/// What the watch for processes that leave the install's group found
/// ([`EscapeWatch`]); nothing on Linux, which needs no watch.
#[cfg(unix)]
#[derive(Default)]
struct Watched {
    /// Every process the watch killed.
    #[cfg(target_os = "macos")]
    killed: std::collections::BTreeSet<libc::pid_t>,
    /// The first error of a scan, when one failed.
    #[cfg(target_os = "macos")]
    failed: Option<String>,
}

/// macOS: while the install runs, a helper thread names the processes that
/// hold its hold file and left its process group, and kills each at once.
///
/// A scan after the install's first process exits is too late: a process
/// that left the group goes on with its work while kGraf looks for it, and
/// at a load average of about 120 the scan and kill after the exit took
/// over 1 s. A process that leaves the group holds the hold file from its
/// fork (it inherits the descriptor), so the watch sees it as soon as it
/// has left.
///
/// Who is an escapee: a holder whose process group is not the install's.
/// The install's first process leads its own session and group (the sandbox
/// calls `setsid`), and every process it starts stays in that group unless
/// it calls `setsid` or `setpgid` itself, which a group kill then cannot
/// reach. The install's own processes, in its group, are never named; nor
/// is kGraf or a child of kGraf. Only processes started since the install
/// began are read (`kgraf_sandbox::lifecycle::holders_started_since`), so a
/// scan takes milliseconds. One the clock leaves out (a clock stepped back)
/// is still found by the final scan after the exit, which reads every
/// process and waits for the hold to be free ([`stop_holders`]).
///
/// Between the scan that names a process and its kill, its PID could only
/// pass to another program if it ended, was reaped, and the PID counter
/// wrapped round to it: macOS hands out PIDs in increasing order up to
/// 99,999.
#[cfg(target_os = "macos")]
struct EscapeWatch {
    done: Arc<std::sync::atomic::AtomicBool>,
    found: Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Watched>,
}

#[cfg(target_os = "macos")]
impl EscapeWatch {
    /// Time between two scans. A scan of new processes takes milliseconds,
    /// so the watch costs little CPU, and a process that left the group is
    /// killed within about this time plus one scan.
    const EVERY: Duration = Duration::from_millis(50);

    /// Watch the holders of `hold` started at or after `since` whose group is
    /// not `group`, the install's.
    fn start(hold: PathBuf, group: libc::pid_t, since: std::time::SystemTime) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let found = Arc::new(AtomicBool::new(false));
        let thread = {
            let (done, found) = (done.clone(), found.clone());
            std::thread::spawn(move || {
                let mut watched = Watched::default();
                while !done.load(Ordering::SeqCst) {
                    match kgraf_sandbox::lifecycle::holders_started_since(&hold, since) {
                        Ok(holders) => {
                            for h in holders.into_iter().filter(|h| h.group != group) {
                                // SAFETY: signals a process that holds the
                                // install's own hold file, which only the
                                // install's processes ever open, that left
                                // the install's group, and that is not a
                                // child of this process.
                                unsafe {
                                    libc::kill(h.pid, libc::SIGKILL);
                                }
                                watched.killed.insert(h.pid);
                                found.store(true, Ordering::SeqCst);
                            }
                        }
                        Err(e) => {
                            watched.failed.get_or_insert_with(|| e.to_string());
                        }
                    }
                    std::thread::sleep(Self::EVERY);
                }
                watched
            })
        };
        Self {
            done,
            found,
            thread,
        }
    }

    /// Whether the watch has killed a process.
    fn found(&self) -> bool {
        self.found.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Stop the watch and say what it found. A watch thread that panicked
    /// is reported as a failed scan.
    fn finish(self) -> Watched {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        self.thread.join().unwrap_or_else(|_| Watched {
            failed: Some("the watch thread ended abnormally".to_owned()),
            ..Watched::default()
        })
    }
}

/// What a failure says when a process that left the group still runs.
#[cfg(unix)]
const STILL_RUNNING: &str = "a process it started left its process group and is still \
     running, and kGraf could not stop it; it can still change files in the project. Find it \
     and stop it before you start again";

/// macOS: kill every process `list` names as a holder of `hold`, again and
/// again until the hold is free or `grace` runs out, and say what happened,
/// naming every process killed here and in `killed` (those the watch killed
/// while the install ran, [`EscapeWatch`]). `None` when the hold is free at
/// once and `killed` is empty.
///
/// Each holder is killed the moment it is named, and the lister reads the
/// process table in this process
/// (`kgraf_sandbox::lifecycle::holders`): a process that left the group goes
/// on with its work while kGraf looks for it, so the time from the group kill
/// to its own kill is the time it has to change files. A listing that names
/// no one is tried again. A hold that frees with no one killed is said as
/// such: the process ended by itself, maybe after it changed files, and kGraf
/// did not stop it.
#[cfg(target_os = "macos")]
fn stop_holders(
    hold: &Path,
    mut list: impl FnMut(&Path) -> std::io::Result<Vec<libc::pid_t>>,
    grace: Duration,
    mut killed: std::collections::BTreeSet<libc::pid_t>,
) -> Option<String> {
    use kgraf_sandbox::lifecycle::hold_free;
    if hold_free(hold) && killed.is_empty() {
        return None;
    }
    let deadline = Instant::now() + grace;
    let mut list_failed = None;
    while !hold_free(hold) && Instant::now() < deadline {
        match list(hold) {
            Ok(pids) => {
                for pid in pids {
                    // SAFETY: signals a process that holds the install's own
                    // hold file, which only the install's processes ever
                    // open, and that is not a child of this process.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    killed.insert(pid);
                }
            }
            Err(e) => list_failed = Some(e),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !hold_free(hold) {
        let why = list_failed
            .map(|e| format!(" (the process list could not be read: {e})"))
            .unwrap_or_default();
        let stopped = if killed.is_empty() {
            String::new()
        } else {
            format!(
                "kGraf stopped {} that left the install's process group, but ",
                pids_text(&killed)
            )
        };
        return Some(format!("{stopped}{STILL_RUNNING}{why}"));
    }
    if killed.is_empty() {
        return Some(
            "a process it started left its process group and kept running until it ended by \
             itself; kGraf did not find it in time to stop it, so it may have changed files in \
             the project. An install that leaves a process behind is not taken as done"
                .to_owned(),
        );
    }
    Some(format!(
        "a process it started left its process group and kept running; kGraf stopped it \
         ({}). An install that leaves a process behind is not taken as done",
        pids_text(&killed)
    ))
}

/// "process 12" or "processes 12, 34".
#[cfg(target_os = "macos")]
fn pids_text(pids: &std::collections::BTreeSet<libc::pid_t>) -> String {
    let named = pids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if pids.len() == 1 {
        format!("process {named}")
    } else {
        format!("processes {named}")
    }
}

/// Wait for the install to end, its timeout, or `stop`; killed at either of
/// the last two, with its whole process group, until every process holding
/// `hold` is gone or the grace runs out. A process that left the group is
/// then handled by [`left_behind`]; on macOS one is killed while the install
/// runs, and the install stopped at once ([`EscapeWatch`]). `spawned` is a
/// time no later than the child's start. Returns whether it succeeded (or
/// why it was stopped) and its output, stdout and stderr interleaved by
/// line, in both cases.
#[cfg(unix)]
fn wait_bounded(
    mut child: std::process::Child,
    timeout: Duration,
    stop: &Stop,
    hold: &Path,
    spawned: std::time::SystemTime,
) -> (Result<bool, String>, String) {
    let streams: Vec<Box<dyn std::io::Read + Send>> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    let (output, readers) = collect_output(streams);
    // The sandboxed child leads its own session and group (the sandbox calls
    // setsid), so a kill takes everything it started.
    let group = match super::group::GroupGuard::register(child.id()) {
        Ok(g) => g,
        Err(e) => {
            // SAFETY: signals only the group the sandboxed child leads, which
            // is unreaped.
            unsafe {
                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
            }
            let _ = child.wait();
            return (
                Err(format!("its process group cannot be tracked: {e}")),
                output_of(&output, readers, Duration::from_secs(5)),
            );
        }
    };
    // A second of margin: a clock stepped back by less still sees every
    // process the install started.
    #[cfg(target_os = "macos")]
    let watch = EscapeWatch::start(
        hold.to_path_buf(),
        child.id() as libc::pid_t,
        spawned
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(std::time::UNIX_EPOCH),
    );
    #[cfg(not(target_os = "macos"))]
    let _ = spawned;
    let deadline = Instant::now() + timeout;
    // Kill the group, again and again, until the hold is free or a short
    // grace passes: a killed member is gone within milliseconds, and one
    // forked after a signal needs its own. A holder still there after that
    // left the group ([`left_behind`]). Not `kill_group_until_hold_free`'s
    // five seconds: a process that left the group could finish its work in
    // that time, unseen.
    let kill_group = |pid: u32| {
        let grace = Instant::now() + Duration::from_millis(250);
        loop {
            // SAFETY: signals only the group the sandboxed child leads,
            // while its leader is unreaped, so the id cannot have been reused.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
            if kgraf_sandbox::lifecycle::hold_free(hold) || Instant::now() >= grace {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let kill = |child: &mut std::process::Child| {
        kill_group(child.id());
        group.release();
        let _ = child.kill();
        let _ = child.wait();
    };
    let result = loop {
        match kgraf_sandbox::lifecycle::exited_unreaped(child.id()) {
            Ok(true) => {
                // As above; a descendant left behind still holds the output
                // pipes, and the readers would wait for it.
                kill_group(child.id());
                group.release();
                break Ok(child.wait().map(|s| s.success()).unwrap_or(false));
            }
            Ok(false) => {}
            Err(e) => {
                kill(&mut child);
                break Err(format!("its state could not be read: {e}"));
            }
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            break Err(format!(
                "it did not finish within {} s and was stopped",
                timeout.as_secs()
            ));
        }
        if stop.is_stopped() {
            kill(&mut child);
            break Err("it was stopped: the run ended or was cancelled".to_owned());
        }
        // The install fails anyway ([`left_behind`] says why): it is not let
        // go on changing the project.
        #[cfg(target_os = "macos")]
        if watch.found() {
            kill(&mut child);
            break Err("a process it started left its process group".to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    #[cfg(target_os = "macos")]
    let watched = watch.finish();
    #[cfg(not(target_os = "macos"))]
    let watched = Watched::default();
    // Before the output is read: a process left behind holds the pipes.
    let result = match left_behind(hold, watched) {
        Some(why) => Err(why),
        None => result,
    };
    (result, output_of(&output, readers, Duration::from_secs(5)))
}

/// Windows: the job object ends the whole tree with the child handle.
#[cfg(not(unix))]
fn wait_bounded(
    mut child: kgraf_sandbox::SandboxedChild,
    timeout: Duration,
    stop: &Stop,
) -> (Result<bool, String>, String) {
    let streams: Vec<Box<dyn std::io::Read + Send>> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    let (output, readers) = collect_output(streams);
    let deadline = Instant::now() + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.success()),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                break Err(format!("its state could not be read: {e}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break Err(format!(
                "it did not finish within {} s and was stopped",
                timeout.as_secs()
            ));
        }
        if stop.is_stopped() {
            let _ = child.kill();
            break Err("it was stopped: the run ended or was cancelled".to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // After a normal exit too: a process the install left behind would hold
    // the pipes. The readers' wait starts once the job holds no process,
    // since a process's pipe ends close only when it is gone.
    let result = match child.kill_tree_and_wait(Duration::from_secs(10)) {
        Ok(true) => result,
        Ok(false) => Err("a process it started was still running after it was stopped".into()),
        Err(e) => Err(format!("its processes could not be ended: {e}")),
    };
    (result, output_of(&output, readers, Duration::from_secs(5)))
}

#[cfg(test)]
mod tests;
