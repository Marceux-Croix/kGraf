//! In-app update check against `Marceux-Croix/kGraf`'s GitHub Releases.
//!
//! **Egress rule (decision 0051, "Local only, single-user").** 0051 permits egress
//! to two destinations only: the model providers the user
//! configured, and the MCP servers the user configured. A GitHub Releases query is
//! neither: it is not a provider call and not an MCP server. It is therefore **opt-in**
//! (`Settings::check_for_updates`, default `false`) rather than a third standing
//! destination; `update_bg.rs`'s background thread never calls [`fetch_releases`] unless
//! the setting is on or the user presses "Check now". The Settings panel and this
//! module's own doc both say so plainly.
//!
//! **Reuse considered and rejected: `axoupdater`.** cargo-dist's own updater (MIT OR
//! Apache-2.0, crates.io 0.10.2, checked 2026-09-28) is the tool decision 0055 and
//! that name. Read at
//! `axoupdater-0.10.2/src/lib.rs::AxoUpdater::run` and `src/receipt.rs`: its `run()`
//! downloads and executes a `{app_name}-installer.sh` (`.ps1` on Windows) asset that
//! **cargo-dist itself generates** at release time, and `load_receipt()` reads an
//! install receipt cargo-dist's own installer script writes on first install. Our
//! release pipeline (`scripts/package-macos.sh`, `package-windows.sh`,
//! `package-linux.sh`; decision 0061's `wixl`-built `.msi` amendment) is not
//! cargo-dist: it produces `.dmg`/`.msi`/`.deb`/`.tar.gz` assets directly and writes no
//! installer script and no receipt. `axoupdater::run()` would hit
//! `AxoupdateError::NoInstallerForPackage` on every release we actually publish. The
//! smallest fitting alternative is used instead: a GitHub
//! Releases API query with the workspace's own `reqwest` client and a `semver`
//! compare, both already workspace dependencies.
//!
//! **Asset naming.** The three packaging scripts fix these names (read directly from
//! each script, 2026-09-28): macOS `kGraf-<version>-macos-universal.dmg` (universal
//! binary, arch-independent); Windows `kGraf-<version>-windows-x86_64.msi` (only
//! `x86_64` is packaged, decision 0061); Linux `kgraf_<version>_<deb_arch>.deb`
//! (`amd64`/`arm64`) or, if that is not published, `kgraf-<version>-linux-<arch>.tar.gz`
//! (`x86_64`/`aarch64`). [`asset_candidates`] is this table.
//!
//! **Checksums.** `scripts/release-all.sh` publishes one
//! shared `SHA256SUMS` asset per release, in the standard `sha256sum` format (`<hex>
//! <filename>` per line, one line per asset); [`find_checksum_asset`] looks for that
//! first. A per-asset `<asset-name>.sha256` file (a bare lowercase hex digest, or the
//! same `sha256sum`-format line) is still accepted as a fallback, for a release built
//! some other way. Three things fail loud, never "download anyway":
//! no checksum asset at all
//! ([`UpdateError::ChecksumMissing`] from [`evaluate`]), a checksum asset with no line
//! for the chosen asset ([`UpdateError::ChecksumMissing`] from [`download_and_verify`]),
//! and a line whose digest does not match the download
//! ([`UpdateError::ChecksumMismatch`]).

use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The project's GitHub repository.
pub const GITHUB_OWNER: &str = "Marceux-Croix";
pub const GITHUB_REPO: &str = "kGraf";
/// The real GitHub API. Tests pass a local `wiremock` server's URI instead; nothing
/// in this module hard-codes this constant into a call site that cannot be overridden.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

fn user_agent() -> String {
    format!("kgraf-app/{}", env!("CARGO_PKG_VERSION"))
}

/// The running build's own version, parsed once. `CARGO_PKG_VERSION` is
/// `version.workspace = true` (`Cargo.toml`), always a valid three-part semver, so a
/// parse failure here would mean the workspace version itself is malformed.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version is valid semver")
}

/// One asset attached to a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// One release, as GitHub's Releases API returns it (only the fields this module
/// reads; `#[serde(default)]` lets a real response carry many more).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// Everything that can keep this check, or an install, from happening. Every variant
/// carries a visible reason (fail loud).
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("no network reaching GitHub Releases: {0}")]
    Network(String),
    #[error("GitHub Releases rate-limited this check; try again later")]
    RateLimited,
    #[error("GitHub Releases returned HTTP {0}")]
    Http(u16),
    #[error("could not read the GitHub Releases response: {0}")]
    Parse(String),
    #[error("no installer is published for this platform ({os}/{arch})")]
    NoAssetForPlatform { os: String, arch: String },
    #[error(
        "release {tag} has no verified checksum for {asset} (no SHA256SUMS entry and no \
         {asset}.sha256); refusing to install unverified"
    )]
    ChecksumMissing { tag: String, asset: String },
    #[error("checksum mismatch for {asset}: release says {expected}, downloaded file is {actual}")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("the checksum file is larger than {max} bytes; refusing to read it")]
    ChecksumFileTooLarge { max: usize },
    #[error("could not build the HTTP client: {0}")]
    Client(String),
    #[error("could not write {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("could not launch the installer: {0}")]
    Launch(String),
}

/// `v0.7.5` and `0.7.5` both parse; anything else is not a version this updater
/// recognizes (a future non-semver tag is skipped, not a hard failure: other releases
/// may still be eligible).
pub fn parse_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// The newest release worth considering: drafts are always excluded (a draft is not
/// published yet); a prerelease is excluded unless `allow_prerelease` is set. Pure.
pub fn latest_eligible(
    releases: &[Release],
    allow_prerelease: bool,
) -> Option<(Version, &Release)> {
    releases
        .iter()
        .filter(|r| !r.draft && (allow_prerelease || !r.prerelease))
        .filter_map(|r| parse_version(&r.tag_name).map(|v| (v, r)))
        .max_by(|a, b| a.0.cmp(&b.0))
}

/// The installer asset names this build's own packaging scripts produce, in the order
/// to prefer them (the `.deb` before the tarball fallback on Linux). `version` is the
/// release's version without its leading `v` (matches the scripts' own `$version`).
pub fn asset_candidates(os: &str, arch: &str, version: &str) -> Result<Vec<String>, UpdateError> {
    let unsupported = || UpdateError::NoAssetForPlatform {
        os: os.to_owned(),
        arch: arch.to_owned(),
    };
    match os {
        // scripts/package-macos.sh: one universal .dmg, arch-independent.
        "macos" => Ok(vec![format!("kGraf-{version}-macos-universal.dmg")]),
        // scripts/package-windows.sh: only x86_64 is packaged (decision 0061).
        "windows" => {
            if arch != "x86_64" {
                return Err(unsupported());
            }
            Ok(vec![format!("kGraf-{version}-windows-x86_64.msi")])
        }
        // scripts/package-linux.sh: a .deb (preferred) and a tarball, per arch.
        "linux" => {
            let deb_arch = match arch {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                _ => return Err(unsupported()),
            };
            Ok(vec![
                format!("kgraf_{version}_{deb_arch}.deb"),
                format!("kgraf-{version}-linux-{arch}.tar.gz"),
            ])
        }
        _ => Err(unsupported()),
    }
}

/// The first published asset, in `asset_candidates`'s preference order, that this
/// platform can install.
pub fn select_asset<'a>(
    release: &'a Release,
    os: &str,
    arch: &str,
) -> Result<&'a Asset, UpdateError> {
    let version =
        parse_version(&release.tag_name).ok_or_else(|| UpdateError::NoAssetForPlatform {
            os: os.to_owned(),
            arch: arch.to_owned(),
        })?;
    let candidates = asset_candidates(os, arch, &version.to_string())?;
    candidates
        .iter()
        .find_map(|name| release.assets.iter().find(|a| &a.name == name))
        .ok_or_else(|| UpdateError::NoAssetForPlatform {
            os: os.to_owned(),
            arch: arch.to_owned(),
        })
}

/// `scripts/release-all.sh`'s own shared checksums file: one per release, covering
/// every asset it publishes.
pub const SHA256SUMS_ASSET_NAME: &str = "SHA256SUMS";

fn checksum_asset_name(asset_name: &str) -> String {
    format!("{asset_name}.sha256")
}

/// The checksum asset to use for `asset_name`: the release's shared `SHA256SUMS`
/// first (`scripts/release-all.sh`'s own mechanism), a per-asset `<asset>.sha256`
/// second (a release built some other way). Neither existing is
/// [`UpdateError::ChecksumMissing`], raised by [`evaluate`]; [`SHA256SUMS_ASSET_NAME`]
/// existing but lacking a line for `asset_name` is a *different* failure, raised by
/// [`download_and_verify`] once the body is actually fetched (this function alone
/// cannot see the file's contents).
fn find_checksum_asset<'a>(release: &'a Release, asset_name: &str) -> Option<&'a Asset> {
    release
        .assets
        .iter()
        .find(|a| a.name == SHA256SUMS_ASSET_NAME)
        .or_else(|| {
            let want = checksum_asset_name(asset_name);
            release.assets.iter().find(|a| a.name == want)
        })
}

/// Reads a `sha256sum`-format body (`<hex>  <filename>`, one or more lines) or a bare
/// hex digest, and returns the digest that names `asset_name` (a bare digest is
/// treated as naming whatever single asset the checksum file was fetched for).
pub fn parse_checksum(body: &str, asset_name: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hex = parts.next()?;
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        match parts.next() {
            None => return Some(hex.to_ascii_lowercase()),
            Some(name) if name.trim_start_matches(['*', ' ']) == asset_name => {
                return Some(hex.to_ascii_lowercase());
            }
            Some(_) => continue,
        }
    }
    None
}

/// A release worth offering: the version, the asset for this platform, and its
/// checksum asset (`None` only when the release truly has none; [`evaluate`] never
/// returns a candidate whose checksum is merely unfetched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCandidate {
    pub version: Version,
    pub tag_name: String,
    pub asset_name: String,
    pub download_url: String,
    pub checksum_name: String,
    pub checksum_url: String,
}

/// The check's pure decision: is there a newer, eligible, installable release? Never
/// touches the network; [`fetch_releases`] supplies `releases`.
pub fn evaluate(
    current: &Version,
    releases: &[Release],
    allow_prerelease: bool,
    os: &str,
    arch: &str,
) -> Result<Option<UpdateCandidate>, UpdateError> {
    let Some((version, release)) = latest_eligible(releases, allow_prerelease) else {
        return Ok(None);
    };
    if &version <= current {
        return Ok(None);
    }
    let asset = select_asset(release, os, arch)?;
    let checksum =
        find_checksum_asset(release, &asset.name).ok_or_else(|| UpdateError::ChecksumMissing {
            tag: release.tag_name.clone(),
            asset: asset.name.clone(),
        })?;
    Ok(Some(UpdateCandidate {
        version,
        tag_name: release.tag_name.clone(),
        asset_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        checksum_name: checksum.name.clone(),
        checksum_url: checksum.browser_download_url.clone(),
    }))
}

/// Queries `{api_base}/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases`. `api_base` is
/// [`GITHUB_API_BASE`] in the real app and a `wiremock::MockServer::uri()` in tests, so
/// no test reaches the real GitHub API.
pub async fn fetch_releases(
    client: &reqwest::Client,
    api_base: &str,
) -> Result<Vec<Release>, UpdateError> {
    let url = format!("{api_base}/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases");
    let resp = client
        .get(url)
        .header("User-Agent", user_agent())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return Err(UpdateError::RateLimited);
    }
    if !status.is_success() {
        return Err(UpdateError::Http(status.as_u16()));
    }
    resp.json::<Vec<Release>>()
        .await
        .map_err(|e| UpdateError::Parse(e.to_string()))
}

/// Downloads `candidate`'s checksum asset, downloads the installer asset into
/// `dest_dir` while hashing it (never buffering the whole file in memory), and refuses
/// it (deleting the partial file) if the hash does not match. Returns the verified
/// installer's path.
pub async fn download_and_verify(
    client: &reqwest::Client,
    candidate: &UpdateCandidate,
    dest_dir: &Path,
) -> Result<PathBuf, UpdateError> {
    let path = dest_dir.join(&candidate.asset_name);
    let mut created = false;
    let result = download_steps(client, candidate, dest_dir, &path, &mut created).await;
    if result.is_err() && created {
        // A failed or stalled download must not leave a partial installer
        // behind for a later run to find. Errors before the file is created
        // leave an earlier verified download alone.
        let _ = std::fs::remove_file(&path);
    }
    result.map(|()| path)
}

/// How long a connection may take to open.
pub const DOWNLOAD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long a download may go without receiving a byte. Not a limit on the
/// whole transfer: an installer of hundreds of megabytes on a slow link
/// finishes as long as data keeps arriving.
pub const DOWNLOAD_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The most bytes read from a checksum file. A `SHA256SUMS` for a release is a
/// few hundred bytes; more than this is not one.
pub const MAX_CHECKSUM_BYTES: usize = 1 << 20;

/// The client for [`download_and_verify`]: a connect timeout and a read-idle
/// timeout, and no whole-request timeout (the release query keeps its own
/// short one). An error when it cannot be built: a default client has neither
/// timeout, so it is never a substitute.
pub fn download_client(
    connect: std::time::Duration,
    idle: std::time::Duration,
) -> Result<reqwest::Client, UpdateError> {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(idle)
        .build()
        .map_err(|e| UpdateError::Client(e.to_string()))
}

/// The download itself. Sets `created` once the destination file exists.
async fn download_steps(
    client: &reqwest::Client,
    candidate: &UpdateCandidate,
    dest_dir: &Path,
    path: &Path,
    created: &mut bool,
) -> Result<(), UpdateError> {
    use tokio::io::AsyncWriteExt as _;

    let mut checksum_resp = client
        .get(&candidate.checksum_url)
        .header("User-Agent", user_agent())
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| UpdateError::Http(e.status().map(|s| s.as_u16()).unwrap_or(0)))?;
    // Read at most MAX_CHECKSUM_BYTES: a server that never stops must not
    // fill memory.
    let mut checksum_bytes = Vec::new();
    while let Some(chunk) = checksum_resp
        .chunk()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
    {
        if checksum_bytes.len() + chunk.len() > MAX_CHECKSUM_BYTES {
            return Err(UpdateError::ChecksumFileTooLarge {
                max: MAX_CHECKSUM_BYTES,
            });
        }
        checksum_bytes.extend_from_slice(&chunk);
    }
    let checksum_body = String::from_utf8_lossy(&checksum_bytes).into_owned();
    let expected = parse_checksum(&checksum_body, &candidate.asset_name).ok_or_else(|| {
        UpdateError::ChecksumMissing {
            tag: candidate.tag_name.clone(),
            asset: candidate.asset_name.clone(),
        }
    })?;

    std::fs::create_dir_all(dest_dir).map_err(|e| UpdateError::Io {
        path: dest_dir.to_owned(),
        message: e.to_string(),
    })?;
    let mut resp = client
        .get(&candidate.download_url)
        .header("User-Agent", user_agent())
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| UpdateError::Http(e.status().map(|s| s.as_u16()).unwrap_or(0)))?;

    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| UpdateError::Io {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
    *created = true;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| UpdateError::Io {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
    }
    file.flush().await.map_err(|e| UpdateError::Io {
        path: path.to_owned(),
        message: e.to_string(),
    })?;
    drop(file);

    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch {
            asset: candidate.asset_name.clone(),
            expected,
            actual,
        });
    }
    Ok(())
}

/// The kind of installer a downloaded, verified file is, from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installer {
    MacDmg(PathBuf),
    WindowsMsi(PathBuf),
    LinuxDeb(PathBuf),
    LinuxTarball(PathBuf),
}

pub fn installer_from_path(path: &Path) -> Option<Installer> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".dmg") {
        Some(Installer::MacDmg(path.to_owned()))
    } else if name.ends_with(".msi") {
        Some(Installer::WindowsMsi(path.to_owned()))
    } else if name.ends_with(".deb") {
        Some(Installer::LinuxDeb(path.to_owned()))
    } else if name.ends_with(".tar.gz") {
        Some(Installer::LinuxTarball(path.to_owned()))
    } else {
        None
    }
}

/// What happened after the verified installer was handed to the operating system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The installer was launched (macOS `open`, Windows `msiexec`); the user
    /// finishes it in that program, not this one.
    Launched,
    /// Nothing was run. The words name the path and the command the user runs
    /// themselves (Linux `.deb`/`.tar.gz`: never `sudo` from this process).
    ManualInstructions(String),
}

/// Runs (or explains) the verified installer. Behind a trait so a test can substitute
/// a recorder instead of a real `open`/`msiexec` process (this crate's own tests never
/// launch a real installer, on any platform).
pub trait Launcher: Send + Sync {
    fn launch(&self, installer: &Installer) -> Result<InstallOutcome, UpdateError>;
}

/// The real launcher: macOS opens the `.dmg` (Finder mounts it and shows the usual
/// drag-to-Applications window); Windows runs `msiexec /i` on the `.msi`; Linux never
/// runs anything, only names the path and the
/// command.
pub struct RealLauncher;

impl Launcher for RealLauncher {
    fn launch(&self, installer: &Installer) -> Result<InstallOutcome, UpdateError> {
        match installer {
            Installer::MacDmg(path) => {
                std::process::Command::new("open")
                    .arg(path)
                    .spawn()
                    .map_err(|e| UpdateError::Launch(e.to_string()))?;
                Ok(InstallOutcome::Launched)
            }
            Installer::WindowsMsi(path) => {
                std::process::Command::new("msiexec")
                    .arg("/i")
                    .arg(path)
                    .spawn()
                    .map_err(|e| UpdateError::Launch(e.to_string()))?;
                Ok(InstallOutcome::Launched)
            }
            Installer::LinuxDeb(path) => Ok(InstallOutcome::ManualInstructions(format!(
                "Downloaded to {}. Install it yourself: sudo apt install {}",
                path.display(),
                path.display()
            ))),
            Installer::LinuxTarball(path) => Ok(InstallOutcome::ManualInstructions(format!(
                "Downloaded to {}. Extract it and replace the installed copy yourself.",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn release(tag: &str, draft: bool, prerelease: bool, assets: Vec<Asset>) -> Release {
        Release {
            tag_name: tag.to_owned(),
            draft,
            prerelease,
            assets,
        }
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_owned(),
            browser_download_url: format!("https://example.test/{name}"),
            size: 1,
        }
    }

    // --- version compare ---

    #[test]
    fn a_higher_tag_beats_a_lower_one_and_v_prefix_is_stripped() {
        let releases = vec![
            release("v0.7.4", false, false, vec![]),
            release("v0.7.10", false, false, vec![]),
            release("0.7.9", false, false, vec![]),
        ];
        let (version, chosen) = latest_eligible(&releases, false).expect("one eligible");
        assert_eq!(version, Version::new(0, 7, 10));
        assert_eq!(chosen.tag_name, "v0.7.10");
    }

    #[test]
    fn evaluate_returns_none_when_the_running_version_is_current_or_newer() {
        let releases = vec![release(
            "v0.7.5",
            false,
            false,
            vec![
                asset("kGraf-0.7.5-macos-universal.dmg"),
                asset("kGraf-0.7.5-macos-universal.dmg.sha256"),
            ],
        )];
        let current = Version::new(0, 7, 5);
        assert!(
            evaluate(&current, &releases, false, "macos", "aarch64")
                .expect("ok")
                .is_none()
        );
        let newer_running = Version::new(0, 8, 0);
        assert!(
            evaluate(&newer_running, &releases, false, "macos", "aarch64")
                .expect("ok")
                .is_none()
        );
    }

    // --- drafts ignored ---

    #[test]
    fn a_draft_is_never_eligible_even_if_newest() {
        let releases = vec![
            release("v0.7.4", false, false, vec![]),
            release("v9.9.9", true, false, vec![]),
        ];
        let (version, _) = latest_eligible(&releases, false).expect("one eligible");
        assert_eq!(version, Version::new(0, 7, 4), "the draft is skipped");
    }

    #[test]
    fn a_prerelease_is_excluded_unless_allowed() {
        let releases = vec![
            release("v0.7.4", false, false, vec![]),
            release("v0.8.0-beta.1", false, true, vec![]),
        ];
        assert_eq!(
            latest_eligible(&releases, false).unwrap().0,
            Version::new(0, 7, 4),
            "prerelease excluded by default"
        );
        assert_eq!(
            latest_eligible(&releases, true).unwrap().0,
            Version::parse("0.8.0-beta.1").unwrap(),
            "prerelease allowed once the setting is on"
        );
    }

    // --- asset selection per OS and arch ---

    #[test]
    fn macos_selects_the_universal_dmg_regardless_of_arch() {
        let names = asset_candidates("macos", "aarch64", "0.7.6").expect("ok");
        assert_eq!(names, vec!["kGraf-0.7.6-macos-universal.dmg"]);
        let names = asset_candidates("macos", "x86_64", "0.7.6").expect("ok");
        assert_eq!(names, vec!["kGraf-0.7.6-macos-universal.dmg"]);
    }

    #[test]
    fn windows_only_supports_x86_64() {
        let names = asset_candidates("windows", "x86_64", "0.7.6").expect("ok");
        assert_eq!(names, vec!["kGraf-0.7.6-windows-x86_64.msi"]);
        assert!(matches!(
            asset_candidates("windows", "aarch64", "0.7.6"),
            Err(UpdateError::NoAssetForPlatform { .. })
        ));
    }

    #[test]
    fn linux_prefers_the_deb_and_falls_back_to_the_tarball() {
        let names = asset_candidates("linux", "x86_64", "0.7.6").expect("ok");
        assert_eq!(
            names,
            vec!["kgraf_0.7.6_amd64.deb", "kgraf-0.7.6-linux-x86_64.tar.gz"]
        );
        let names = asset_candidates("linux", "aarch64", "0.7.6").expect("ok");
        assert_eq!(
            names,
            vec!["kgraf_0.7.6_arm64.deb", "kgraf-0.7.6-linux-aarch64.tar.gz"]
        );

        let r = release(
            "v0.7.6",
            false,
            false,
            vec![asset("kgraf-0.7.6-linux-x86_64.tar.gz")],
        );
        let chosen = select_asset(&r, "linux", "x86_64").expect("tarball fallback");
        assert_eq!(chosen.name, "kgraf-0.7.6-linux-x86_64.tar.gz");
    }

    #[test]
    fn an_unsupported_platform_is_a_visible_error_not_a_silent_none() {
        let err = asset_candidates("freebsd", "x86_64", "0.7.6").expect_err("unsupported");
        assert!(matches!(err, UpdateError::NoAssetForPlatform { .. }));
        assert!(err.to_string().contains("freebsd"));
    }

    #[test]
    fn no_matching_asset_in_a_real_release_is_a_visible_error() {
        let r = release("v0.7.6", false, false, vec![asset("something-else.zip")]);
        let err = select_asset(&r, "macos", "aarch64").expect_err("no match");
        assert!(matches!(err, UpdateError::NoAssetForPlatform { .. }));
    }

    // --- checksum ---

    #[test]
    fn parse_checksum_reads_a_bare_digest_and_a_sha256sum_line() {
        let hex = "a".repeat(64);
        assert_eq!(parse_checksum(&hex, "asset.dmg"), Some(hex.clone()));
        let line = format!("{hex}  asset.dmg\n");
        assert_eq!(parse_checksum(&line, "asset.dmg"), Some(hex.clone()));
        // A multi-asset SHA256SUMS: only the matching line counts.
        let other = "b".repeat(64);
        let multi = format!("{other}  other.dmg\n{hex}  asset.dmg\n");
        assert_eq!(parse_checksum(&multi, "asset.dmg"), Some(hex));
        assert_eq!(parse_checksum("not a checksum at all", "asset.dmg"), None);
    }

    /// Mutation check on the lookup: a name that merely starts with `asset.dmg`
    /// (`asset.dmg.sig`) or contains it must never satisfy a lookup for `asset.dmg`
    /// itself; only an exact filename match does, regardless of which line it is on.
    #[test]
    fn parse_checksum_matches_the_exact_filename_only_not_a_prefix_or_substring() {
        let wrong = "b".repeat(64);
        let right = "c".repeat(64);
        let body = format!(
            "{wrong}  asset.dmg.sig\n{wrong}  notasset.dmg\n{right}  asset.dmg\n{wrong}  asset.dmg.old\n"
        );
        assert_eq!(parse_checksum(&body, "asset.dmg"), Some(right));
    }

    #[test]
    fn release_with_no_checksum_asset_at_all_is_refused_before_any_download() {
        let releases = vec![release(
            "v9.9.9",
            false,
            false,
            vec![asset("kGraf-9.9.9-macos-universal.dmg")], // no SHA256SUMS, no .sha256
        )];
        let err = evaluate(&Version::new(0, 1, 0), &releases, false, "macos", "aarch64")
            .expect_err("refused");
        assert!(matches!(err, UpdateError::ChecksumMissing { .. }));
        assert!(err.to_string().contains("SHA256SUMS"));
    }

    /// `scripts/release-all.sh`'s own mechanism: one shared `SHA256SUMS` asset per
    /// release, covering every installer it publishes.
    #[test]
    fn evaluate_prefers_the_shared_sha256sums_asset() {
        let releases = vec![release(
            "v9.9.9",
            false,
            false,
            vec![
                asset("kGraf-9.9.9-macos-universal.dmg"),
                asset(SHA256SUMS_ASSET_NAME),
            ],
        )];
        let candidate = evaluate(&Version::new(0, 1, 0), &releases, false, "macos", "aarch64")
            .expect("ok")
            .expect("a candidate");
        assert_eq!(candidate.asset_name, "kGraf-9.9.9-macos-universal.dmg");
        assert_eq!(candidate.checksum_name, SHA256SUMS_ASSET_NAME);
    }

    #[test]
    fn evaluate_falls_back_to_a_per_asset_sha256_when_there_is_no_sha256sums() {
        let releases = vec![release(
            "v9.9.9",
            false,
            false,
            vec![
                asset("kGraf-9.9.9-macos-universal.dmg"),
                asset("kGraf-9.9.9-macos-universal.dmg.sha256"),
            ],
        )];
        let candidate = evaluate(&Version::new(0, 1, 0), &releases, false, "macos", "aarch64")
            .expect("ok")
            .expect("a candidate");
        assert_eq!(candidate.asset_name, "kGraf-9.9.9-macos-universal.dmg");
        assert_eq!(
            candidate.checksum_name,
            "kGraf-9.9.9-macos-universal.dmg.sha256"
        );
    }

    /// Mutation check on the lookup order: with both a `SHA256SUMS` asset and a
    /// per-asset `.sha256` present, `SHA256SUMS` must win (it is the mechanism
    /// `scripts/release-all.sh` actually uses); swapping the `.or_else` branches in
    /// `find_checksum_asset` fails this test.
    #[test]
    fn evaluate_prefers_sha256sums_even_when_a_per_asset_file_is_also_present() {
        let releases = vec![release(
            "v9.9.9",
            false,
            false,
            vec![
                asset("kGraf-9.9.9-macos-universal.dmg"),
                asset("kGraf-9.9.9-macos-universal.dmg.sha256"),
                asset(SHA256SUMS_ASSET_NAME),
            ],
        )];
        let candidate = evaluate(&Version::new(0, 1, 0), &releases, false, "macos", "aarch64")
            .expect("ok")
            .expect("a candidate");
        assert_eq!(candidate.checksum_name, SHA256SUMS_ASSET_NAME);
    }

    // --- network: a local fake server only, never the real GitHub API ---

    #[tokio::test]
    async fn fetch_releases_parses_a_real_response_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"tag_name": "v0.7.6", "draft": false, "prerelease": false, "assets": []}
            ])))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let releases = fetch_releases(&client, &server.uri()).await.expect("ok");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].tag_name, "v0.7.6");
    }

    #[tokio::test]
    async fn fetch_releases_reports_rate_limiting_loudly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let err = fetch_releases(&client, &server.uri())
            .await
            .expect_err("rate limited");
        assert!(matches!(err, UpdateError::RateLimited));
    }

    #[tokio::test]
    async fn fetch_releases_reports_a_network_failure_loudly() {
        // No server listening at this address at all.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .expect("client");
        let err = fetch_releases(&client, "http://127.0.0.1:1")
            .await
            .expect_err("no server");
        assert!(matches!(err, UpdateError::Network(_)));
    }

    /// The fallback path: a per-asset `.sha256` file, for a release built without
    /// `scripts/release-all.sh`'s shared `SHA256SUMS`.
    #[tokio::test]
    async fn download_and_verify_accepts_a_matching_checksum() {
        let server = MockServer::start().await;
        let body = b"pretend installer bytes";
        let hex: String = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz.sha256"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!("{hex}  asset.tar.gz\n")),
            )
            .mount(&server)
            .await;
        let candidate = UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{}/asset.tar.gz", server.uri()),
            checksum_name: "asset.tar.gz.sha256".into(),
            checksum_url: format!("{}/asset.tar.gz.sha256", server.uri()),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let path = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect("verified");
        assert_eq!(std::fs::read(&path).expect("read"), body);
    }

    /// A one-thread HTTP server on a local port: `/sum` returns `sum_body`;
    /// `/asset` sends `chunks` with `gap` between them, then, when `stall` is
    /// set, holds the connection open without finishing the body. Returns the
    /// base URL. The thread ends with the process; each test binds its own.
    fn drip_server(
        sum_body: String,
        chunks: Vec<Vec<u8>>,
        gap: std::time::Duration,
        stall: bool,
    ) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (sum_body, chunks) = (sum_body.clone(), chunks.clone());
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 512];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let first = String::from_utf8_lossy(&req);
                    if first.starts_with("GET /sum") {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sum_body}",
                            sum_body.len()
                        );
                        return;
                    }
                    let total: usize = chunks.iter().map(Vec::len).sum();
                    let declared = if stall { total + 1000 } else { total };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                    );
                    for chunk in &chunks {
                        if stream.write_all(chunk).is_err() || stream.flush().is_err() {
                            return;
                        }
                        std::thread::sleep(gap);
                    }
                    if stall {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                    }
                });
            }
        });
        base
    }

    fn drip_candidate(base: &str) -> UpdateCandidate {
        UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{base}/asset"),
            checksum_name: "asset.tar.gz.sha256".into(),
            checksum_url: format!("{base}/sum"),
        }
    }

    /// A download that keeps receiving bytes finishes even when the whole
    /// transfer takes longer than a whole-request timeout would allow. The
    /// control client, with that whole-request timeout, fails on the same
    /// server, so the test proves the difference between the two.
    #[tokio::test]
    async fn a_slow_but_steady_download_is_not_cut_off_by_the_idle_timeout() {
        use std::time::Duration;
        let chunks: Vec<Vec<u8>> = (0..8).map(|i| vec![b'a' + i as u8; 64]).collect();
        let body: Vec<u8> = chunks.concat();
        let hex: String = Sha256::digest(&body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let base = drip_server(
            format!("{hex}  asset.tar.gz\n"),
            chunks,
            Duration::from_millis(150),
            false,
        );
        let candidate = drip_candidate(&base);

        let whole = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let err = download_and_verify(&whole, &candidate, dir.path())
            .await
            .expect_err("a whole-request timeout cuts a slow transfer");
        assert!(matches!(err, UpdateError::Network(_)), "{err}");

        let client =
            download_client(Duration::from_secs(2), Duration::from_millis(600)).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect("steady progress is not idle");
        assert_eq!(std::fs::read(path).expect("read"), body);
    }

    /// A checksum file over the cap is refused before the installer is
    /// fetched, and nothing is written.
    #[tokio::test]
    async fn an_oversized_checksum_file_is_refused_before_the_download() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/SHA256SUMS"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'a'; MAX_CHECKSUM_BYTES + 1]),
            )
            .mount(&server)
            .await;
        let installer = Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .expect(0);
        installer.mount(&server).await;
        let mut candidate = drip_candidate(&server.uri());
        candidate.checksum_url = format!("{}/SHA256SUMS", server.uri());
        candidate.download_url = format!("{}/asset.tar.gz", server.uri());
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let err = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect_err("an oversized checksum file");
        assert!(
            matches!(err, UpdateError::ChecksumFileTooLarge { .. }),
            "{err}"
        );
        assert!(!dir.path().join("asset.tar.gz").exists());
    }

    /// A download that stops sending fails after the idle timeout, and the
    /// partial file is deleted.
    #[tokio::test]
    async fn a_stalled_download_fails_and_leaves_no_partial_file() {
        use std::time::Duration;
        let base = drip_server(
            format!("{}  asset.tar.gz\n", "0".repeat(64)),
            vec![vec![b'x'; 100]],
            Duration::from_millis(10),
            true,
        );
        let candidate = drip_candidate(&base);
        let client =
            download_client(Duration::from_secs(2), Duration::from_millis(300)).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let started = std::time::Instant::now();
        let err = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect_err("a stalled body must fail");
        assert!(matches!(err, UpdateError::Network(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            !dir.path().join("asset.tar.gz").exists(),
            "the partial installer was left behind"
        );
    }

    /// The mechanism under test: a checksum mismatch is
    /// refused, and the bad file is not left behind for anything to run later.
    #[tokio::test]
    async fn download_and_verify_refuses_a_checksum_mismatch_and_deletes_the_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"real bytes".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz.sha256"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{}  asset.tar.gz\n", "0".repeat(64))),
            )
            .mount(&server)
            .await;
        let candidate = UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{}/asset.tar.gz", server.uri()),
            checksum_name: "asset.tar.gz.sha256".into(),
            checksum_url: format!("{}/asset.tar.gz.sha256", server.uri()),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let err = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect_err("mismatch");
        assert!(matches!(err, UpdateError::ChecksumMismatch { .. }));
        assert!(
            !dir.path().join("asset.tar.gz").exists(),
            "the bad file is removed"
        );
    }

    // --- SHA256SUMS: the shared checksums file `scripts/release-all.sh` publishes
    // (without it, every update would be refused against the
    // real release pipeline). Success, and all three fail-loud cases:
    // the file missing, the line missing, and the hash mismatching.

    /// Success: a `SHA256SUMS` covering several assets, only one of which is the
    /// one this platform picked; the right line is found regardless of its position
    /// in the file.
    #[tokio::test]
    async fn download_and_verify_accepts_a_matching_line_from_a_real_sha256sums() {
        let server = MockServer::start().await;
        let body = b"pretend installer bytes";
        let hex: String = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let other_hex = "d".repeat(64);
        let sums = format!(
            "{other_hex}  kgraf_9.9.9_amd64.deb\n{hex}  asset.tar.gz\n{other_hex}  kGraf-9.9.9-windows-x86_64.msi\n"
        );
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{SHA256SUMS_ASSET_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(&server)
            .await;
        let candidate = UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{}/asset.tar.gz", server.uri()),
            checksum_name: SHA256SUMS_ASSET_NAME.into(),
            checksum_url: format!("{}/{SHA256SUMS_ASSET_NAME}", server.uri()),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let path = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect("verified against SHA256SUMS");
        assert_eq!(std::fs::read(&path).expect("read"), body);
    }

    /// Fail loud, case 1: the checksum asset itself is missing from the release
    /// (the `SHA256SUMS` URL 404s). Caught here, not merely at `evaluate` time,
    /// because a stale `UpdateCandidate` (built from an earlier listing) could in
    /// principle point at an asset that was since removed.
    #[tokio::test]
    async fn download_and_verify_refuses_when_the_checksums_file_itself_is_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .mount(&server)
            .await;
        // No mock for `/SHA256SUMS` at all: wiremock 404s it.
        let candidate = UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{}/asset.tar.gz", server.uri()),
            checksum_name: SHA256SUMS_ASSET_NAME.into(),
            checksum_url: format!("{}/{SHA256SUMS_ASSET_NAME}", server.uri()),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let err = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect_err("missing checksums file");
        assert!(matches!(err, UpdateError::Http(404)));
    }

    /// Fail loud, case 2: `SHA256SUMS` exists and downloads fine, but has no line
    /// naming the chosen asset (a release built oddly, or a name mismatch).
    #[tokio::test]
    async fn download_and_verify_refuses_when_sha256sums_has_no_line_for_the_asset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{SHA256SUMS_ASSET_NAME}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{}  some-other-asset.deb\n", "e".repeat(64))),
            )
            .mount(&server)
            .await;
        let candidate = UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{}/asset.tar.gz", server.uri()),
            checksum_name: SHA256SUMS_ASSET_NAME.into(),
            checksum_url: format!("{}/{SHA256SUMS_ASSET_NAME}", server.uri()),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let err = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect_err("no line for this asset");
        assert!(matches!(err, UpdateError::ChecksumMissing { .. }));
        assert!(
            !dir.path().join("asset.tar.gz").exists(),
            "nothing is written for an asset SHA256SUMS never named"
        );
    }

    /// Fail loud, case 3: `SHA256SUMS` has a line for the asset, but the digest does
    /// not match what was actually downloaded.
    #[tokio::test]
    async fn download_and_verify_refuses_a_sha256sums_mismatch_and_deletes_the_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"real bytes".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{SHA256SUMS_ASSET_NAME}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{}  asset.tar.gz\n", "0".repeat(64))),
            )
            .mount(&server)
            .await;
        let candidate = UpdateCandidate {
            version: Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "asset.tar.gz".into(),
            download_url: format!("{}/asset.tar.gz", server.uri()),
            checksum_name: SHA256SUMS_ASSET_NAME.into(),
            checksum_url: format!("{}/{SHA256SUMS_ASSET_NAME}", server.uri()),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let err = download_and_verify(&client, &candidate, dir.path())
            .await
            .expect_err("mismatch");
        assert!(matches!(err, UpdateError::ChecksumMismatch { .. }));
        assert!(
            !dir.path().join("asset.tar.gz").exists(),
            "the bad file is removed"
        );
    }

    #[tokio::test]
    async fn a_release_with_no_asset_for_this_platform_is_a_visible_error() {
        let releases = vec![release(
            "v9.9.9",
            false,
            false,
            vec![
                asset("kgraf_9.9.9_amd64.deb"),
                asset("kgraf_9.9.9_amd64.deb.sha256"),
            ],
        )];
        // A Windows arm64 build (unsupported, decision 0061) sees no usable asset.
        let err = evaluate(
            &Version::new(0, 1, 0),
            &releases,
            false,
            "windows",
            "aarch64",
        )
        .expect_err("no asset");
        assert!(matches!(err, UpdateError::NoAssetForPlatform { .. }));
    }

    // --- installer selection and the launcher trait ---

    #[test]
    fn installer_from_path_matches_the_four_kinds() {
        assert_eq!(
            installer_from_path(Path::new("/x/kGraf-1.dmg")),
            Some(Installer::MacDmg("/x/kGraf-1.dmg".into()))
        );
        assert_eq!(
            installer_from_path(Path::new("/x/kGraf-1.msi")),
            Some(Installer::WindowsMsi("/x/kGraf-1.msi".into()))
        );
        assert_eq!(
            installer_from_path(Path::new("/x/kgraf_1.deb")),
            Some(Installer::LinuxDeb("/x/kgraf_1.deb".into()))
        );
        assert_eq!(
            installer_from_path(Path::new("/x/kgraf-1.tar.gz")),
            Some(Installer::LinuxTarball("/x/kgraf-1.tar.gz".into()))
        );
        assert_eq!(installer_from_path(Path::new("/x/readme.txt")), None);
    }

    /// The Linux path never runs anything; it
    /// only names the path and the command, and does not touch `Command::new` at all.
    #[test]
    fn the_linux_deb_launcher_never_spawns_a_process() {
        let outcome = RealLauncher
            .launch(&Installer::LinuxDeb("/tmp/kgraf.deb".into()))
            .expect("ok");
        let InstallOutcome::ManualInstructions(words) = outcome else {
            panic!("must be manual instructions, never a launch, for a .deb");
        };
        assert!(words.contains("/tmp/kgraf.deb"));
        assert!(words.contains("sudo apt install /tmp/kgraf.deb"));
    }
}
