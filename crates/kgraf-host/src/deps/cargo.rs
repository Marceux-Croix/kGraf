//! crates.io through cargo: the offline check, the fetch, and the manifest
//! mirror both run in.
//!
//! **The mirror.** Both commands run on a copy of every `Cargo.toml` of the
//! project (and its root `Cargo.lock`), each with an empty `src/lib.rs`
//! beside it, in a fresh private directory under the checkout's state
//! directory (`<checkout>/.kgraf/deps-mirror/kgraf-deps-*`). Measured
//! (cargo 1.95.0): cargo loads a manifest with missing explicit target paths,
//! build script or readme; it needs only an auto-discovered target, which the
//! stub gives. So:
//!
//! - cargo writes its lockfile into the mirror, never into the user's
//!   checkout (`cargo metadata` writes one when it is absent or stale);
//! - cargo and rustup look for their configuration, toolchain file and
//!   workspace root in the mirror's parent folders. Those are the state
//!   directory (which the workers' sandbox can neither read nor write) and
//!   then the checkout, so they find the checkout's own `.cargo/config.toml`
//!   and `rust-toolchain.toml`, as they would in the project, and a relative
//!   path in that configuration (a vendored source folder) resolves in the
//!   checkout. A folder in the system temporary directory would instead let
//!   any file planted above it (a `build.rustc-wrapper`, a `path` toolchain,
//!   a `[workspace]` with patches) apply to the host's cargo;
//! - the mirrored root manifest is made a workspace root (an empty
//!   `[workspace]` is appended) when it has no `[workspace]` and names none
//!   with `package.workspace`. Measured: cargo otherwise reads each parent
//!   folder's `Cargo.toml` as the workspace root, so the checkout's own
//!   manifest, which is not mirrored or checked, would apply. A sentinel
//!   parent that excludes the copy does not stop that climb;
//! - a worker's worktree is only ever read, through a directory capability
//!   (`cap_std`) that cannot resolve outside it, and a symlink in it is
//!   skipped, never followed; no host process writes there;
//! - the worktree's own `.cargo` configuration and toolchain files are not
//!   copied (hidden folders and non-manifest files never are): they could
//!   name a registry, a credential program or a toolchain of the worker's
//!   choosing;
//! - a manifest path that leaves the project (a `path`, `workspace` or member
//!   entry that is absolute or climbs above the root) would resolve against
//!   the mirror's parents, where cargo would read whatever manifest is there
//!   (measured: `cargo metadata --no-deps` read `<parent>/near/Cargo.toml`
//!   for `path = "../near"`), so with any, cargo is not run on the mirror.
//!
//! **The check** is `cargo metadata --offline`: it resolves every dependency
//! for every platform and reads each one's manifest, so it fails at once when
//! a crate is not downloaded and unpacked. On this repository (802 packages,
//! all cached) it takes about 245 ms.
//!
//! **Sources.** Consent covers crates.io only, so a fetch is refused when any
//! mirrored manifest, member or not, has a `git`, `registry` or
//! `registry-index` key in any table but the `metadata` tables cargo ignores
//! (`package.metadata`, `project.metadata`, `workspace.metadata`): a dependency,
//! a `[patch]` or `[replace]` entry, a `[workspace.dependencies]` entry.
//! Every manifest counts because cargo reads more than the members: a
//! `[patch]` path into an excluded folder, a path dependency there, or a
//! workspace root named by `package.workspace` each bring in a manifest that
//! `cargo metadata --no-deps` does not list. Then, as a second reading,
//! `cargo metadata --no-deps --offline` lists each member's declared
//! dependencies with their sources, and the root manifest's `[patch]` and
//! `[replace]` tables are read.
//!
//! **No git transport.** Every command runs with
//! `--config net.git-fetch-with-cli=true` and `GIT_ALLOW_PROTOCOL=none`:
//! cargo then fetches git sources with the `git` program, which refuses every
//! transport not listed in that variable (`file`, `ssh`, `https` alike). So
//! even a git source the reading above missed is never cloned into the cargo
//! home, which the workers' sandbox can read (a `file://` source would copy a
//! local repository there). A crates.io index configured as a git index is
//! refused the same way; the default sparse index is plain HTTPS.

use super::{DependencyTool, DepsError, FetchReport, Readiness, Registry, Stop, redact};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Where the dependencies are declared and whose cargo settings apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchSite {
    /// The directory holding the root `Cargo.toml`: the project's checkout
    /// at run start, a worker's worktree when a worker needs a dependency.
    pub source: PathBuf,
    /// The user's checkout: the mirror is made under its state directory, so
    /// its `.cargo/config.toml` and toolchain file are the ones cargo uses.
    pub checkout: PathBuf,
}

impl FetchSite {
    /// The project itself (run start).
    pub fn project(root: &Path) -> Self {
        Self {
            source: root.to_path_buf(),
            checkout: root.to_path_buf(),
        }
    }
}

/// The crates.io registry index, as cargo names it in a dependency's source.
const CRATES_IO_SOURCES: [&str; 2] = [
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

/// What cargo prints when a dependency is not ready offline (the words of
/// decision 0084's hint, and the resolver's).
const NOT_READY: &[&str] = &[
    "no matching package named",
    "failed to download",
    "failed to unpack package",
    "--offline was specified",
    "you're using offline mode",
    "failed to load source for dependency",
    "failed to get `",
];

/// Folders the mirror does not enter: build output, other ecosystems'
/// packages, and every hidden folder (`.git`, `.kgraf`, `.cargo`).
const SKIP_DIRS: [&str; 2] = ["target", "node_modules"];
const MAX_DEPTH: usize = 24;
const MAX_MANIFESTS: usize = 5_000;
const MAX_ENTRIES: usize = 500_000;
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_LOCK_BYTES: u64 = 16 << 20;
/// The folder under the checkout's state directory that holds the mirrors.
pub(crate) const MIRROR_DIR: &str = "deps-mirror";
/// A mirror left by a process that died is removed by a later build after
/// this long (a live one is removed by its own guard within minutes).
const STALE_MIRROR: Duration = Duration::from_secs(24 * 60 * 60);
/// Most packages one fetch downloads before it is killed. This repository
/// resolves 802; the bound is for a fetch gone wrong (a manifest naming
/// thousands of crates), not for a real project.
const MAX_DOWNLOADS_PER_FETCH: usize = 3_000;
/// The search for a nested manifest when the root has none: shallow and
/// small, it only decides whether to say so.
const NESTED_DEPTH: usize = 6;
const NESTED_ENTRIES: usize = 50_000;

/// The real crates.io tool.
#[derive(Clone, Debug)]
pub struct CargoDeps {
    cargo: PathBuf,
    check_timeout: Duration,
    fetch_timeout: Duration,
    max_downloads: usize,
    /// Tests only: `CARGO_NET_OFFLINE=true` on every command, so no test can
    /// reach a registry even if the code under test is wrong.
    #[cfg(test)]
    offline_only: bool,
}

impl Default for CargoDeps {
    fn default() -> Self {
        Self {
            cargo: PathBuf::from("cargo"),
            check_timeout: Duration::from_secs(120),
            fetch_timeout: Duration::from_secs(20 * 60),
            max_downloads: MAX_DOWNLOADS_PER_FETCH,
            #[cfg(test)]
            offline_only: false,
        }
    }
}

impl CargoDeps {
    fn command(&self, mirror: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.cargo);
        // See the module docs, "No git transport".
        cmd.args(["--config", "net.git-fetch-with-cli=true"])
            .args(args)
            .env("GIT_ALLOW_PROTOCOL", "none")
            // The consent covers crates, not a Rust toolchain. Without this,
            // the rustup `cargo` shim downloads the toolchain a project's
            // `rust-toolchain.toml` names before any consent is asked. Set,
            // the shim stops with "toolchain is not installed" instead. A
            // rustup older than 1.28 ignores the variable.
            .env("RUSTUP_AUTO_INSTALL", "0")
            .current_dir(mirror)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(test)]
        if self.offline_only {
            cmd.env("CARGO_NET_OFFLINE", "true");
        }
        cmd
    }

    /// Run cargo in `mirror` to its end, its timeout, `watch`'s stop or its
    /// download bound; the mirror's path in its output reads `<mirror>`.
    fn run(
        &self,
        mirror: &Path,
        args: &[&str],
        timeout: Duration,
        watch: Watch<'_>,
    ) -> std::io::Result<Output> {
        let out = run_bounded(self.command(mirror, args), timeout, watch)?;
        let clean = |text: &str| redact(text, &[(mirror, "<mirror>")]);
        Ok(Output {
            ok: out.ok,
            stdout: out.stdout,
            stderr: clean(&out.stderr),
        })
    }

    /// The declared sources that are not crates.io, from cargo's own reading
    /// of the members' manifests and from the root manifest's patch tables.
    fn other_sources(&self, mirror: &Path) -> Result<Vec<String>, DepsError> {
        let out = self
            .run(
                mirror,
                &[
                    "metadata",
                    "--no-deps",
                    "--offline",
                    "--format-version",
                    "1",
                ],
                self.check_timeout,
                Watch::default(),
            )
            .map_err(|e| fetch_error(format!("cannot list the manifests' sources: {e}")))?;
        if !out.ok {
            return Err(fetch_error(format!(
                "cannot list the manifests' sources: {}",
                tail(&out.stderr)
            )));
        }
        let roots = [
            mirror.to_path_buf(),
            mirror
                .canonicalize()
                .map_err(|e| DepsError::io("cannot resolve the manifest mirror", e))?,
        ];
        let mut found = declared_other_sources(&out.stdout, &roots)?;
        let root = std::fs::read_to_string(mirror.join("Cargo.toml"))
            .map_err(|e| DepsError::io("cannot read the mirrored root manifest", e))?;
        found.extend(patched_other_sources(&root));
        found.sort();
        found.dedup();
        Ok(found)
    }
}

impl DependencyTool for CargoDeps {
    fn registry(&self) -> Registry {
        Registry::CratesIo
    }

    fn applies(&self, site: &FetchSite) -> bool {
        site.source.join("Cargo.toml").is_file()
    }

    fn nested_manifest(&self, site: &FetchSite) -> Option<PathBuf> {
        find_nested_manifest(&site.source)
    }

    fn check(&self, site: &FetchSite) -> Readiness {
        self.check_watched(site, &Stop::default())
    }

    fn check_watched(&self, site: &FetchSite, stop: &Stop) -> Readiness {
        let mirror = match Mirror::build(site) {
            Ok(m) => m,
            Err(e) => return Readiness::Unchecked(e.to_string()),
        };
        if !mirror.found.escapes.is_empty() {
            return Readiness::Unchecked(format!(
                "the manifests name paths outside the project, which kGraf does not follow: {}",
                mirror.found.escapes.join(", ")
            ));
        }
        // A git or other-registry source does not stop the check: it runs
        // offline and clones nothing. The fetch refuses such a source.
        let out = match self.run(
            mirror.path(),
            &["metadata", "--offline", "--format-version", "1"],
            self.check_timeout,
            Watch {
                stop: Some(stop),
                max_downloads: None,
            },
        ) {
            Ok(out) => out,
            Err(e) => return Readiness::Unchecked(format!("cargo metadata: {e}")),
        };
        classify(&out)
    }

    fn fetch(&self, site: &FetchSite, stop: &Stop) -> Result<FetchReport, DepsError> {
        let mirror = Mirror::build(site)?;
        let refuse = |sources: Vec<String>| DepsError::OtherSources {
            registry: Registry::CratesIo,
            project: site.source.clone(),
            sources,
        };
        let found = mirror.found.escapes.iter().chain(&mirror.found.sources);
        let found: Vec<String> = found.cloned().collect();
        if !found.is_empty() {
            return Err(refuse(found));
        }
        let others = self.other_sources(mirror.path())?;
        if !others.is_empty() {
            return Err(refuse(others));
        }
        let started = Instant::now();
        let watch = Watch {
            stop: Some(stop),
            max_downloads: Some(self.max_downloads),
        };
        let out = self
            .run(mirror.path(), &["fetch"], self.fetch_timeout, watch)
            .map_err(|e| fetch_error(format!("cargo fetch: {e}")))?;
        if !out.ok {
            return Err(fetch_error(format!("cargo fetch: {}", tail(&out.stderr))));
        }
        Ok(report_of(&out.stderr, started.elapsed()))
    }
}

fn fetch_error(detail: String) -> DepsError {
    DepsError::Fetch {
        registry: Registry::CratesIo,
        detail,
    }
}

/// A finished command's result.
struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// The offline check's verdict from cargo's exit and words.
fn classify(out: &Output) -> Readiness {
    if out.ok {
        Readiness::Ready
    } else if out.stderr.contains("toolchain") && out.stderr.contains("is not installed") {
        // rustup's refusal to download it (`RUSTUP_AUTO_INSTALL=0`): nothing
        // a dependency fetch can fix.
        Readiness::Unchecked(format!(
            "the project's Rust toolchain is not installed, and kGraf never downloads one: run \
             `rustup toolchain install` in the project yourself. cargo said: {}",
            tail(&out.stderr)
        ))
    } else if NOT_READY.iter().any(|p| out.stderr.contains(p)) {
        Readiness::Missing(tail(&out.stderr))
    } else {
        Readiness::Unchecked(format!("cargo metadata failed: {}", tail(&out.stderr)))
    }
}

/// The fetch's report from cargo's progress lines on stderr.
fn report_of(stderr: &str, elapsed: Duration) -> FetchReport {
    let lines = stderr.lines().map(str::trim);
    // Every "Updating ..." line, not only "... index" ones: a git source
    // ("Updating git repository `...`") or any other host cargo names must
    // never be left out of the report.
    let contacted = lines
        .clone()
        .filter(|l| l.starts_with("Updating "))
        .map(str::to_owned)
        .collect();
    let downloaded = lines.filter(|l| is_package_download(l)).count();
    FetchReport {
        registry: Registry::CratesIo,
        contacted,
        downloaded,
        elapsed,
    }
}

/// Whether a trimmed cargo line reports one downloaded package: one
/// "Downloaded <name> v<version>" per package, and one summary line
/// ("Downloaded 3 crates (1.2 MB) in 0.5s") that is not a package.
fn is_package_download(line: &str) -> bool {
    line.starts_with("Downloaded ") && !line.contains(" crates (") && !line.contains(" crate (")
}

/// The last lines of cargo's error, enough to name the missing package.
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(6);
    lines[start..].join(" / ")
}

/// Every declared dependency source in `cargo metadata --no-deps` output that
/// is not crates.io: a git repository or another registry. Also a member, or a
/// path dependency, outside the mirror (`roots`: its path as given and
/// resolved): cargo would read that manifest on the host and fetch what it
/// names, and it is not the project's.
fn declared_other_sources(metadata: &str, roots: &[PathBuf]) -> Result<Vec<String>, DepsError> {
    let doc: serde_json::Value = serde_json::from_str(metadata)
        .map_err(|e| fetch_error(format!("cargo metadata output is not JSON: {e}")))?;
    let inside = |p: &str| roots.iter().any(|r| Path::new(p).starts_with(r));
    let mut found = Vec::new();
    for package in doc["packages"].as_array().into_iter().flatten() {
        if let Some(manifest) = package["manifest_path"].as_str()
            && !inside(manifest)
        {
            let name = package["name"].as_str().unwrap_or("?");
            found.push(format!("member {name} outside the project ({manifest})"));
        }
        for dep in package["dependencies"].as_array().into_iter().flatten() {
            if let Some(path) = dep["path"].as_str()
                && !inside(path)
            {
                let name = dep["name"].as_str().unwrap_or("?");
                found.push(format!("{name} from a path outside the project ({path})"));
            }
            let name = dep["name"].as_str().unwrap_or("?");
            if let Some(source) = dep["source"].as_str()
                && !CRATES_IO_SOURCES.contains(&source)
            {
                found.push(format!("{name} from {source}"));
            }
            if let Some(registry) = dep["registry"].as_str() {
                found.push(format!("{name} from registry {registry}"));
            }
        }
    }
    Ok(found)
}

/// `[patch.<source>]` and `[replace]` entries of the root manifest that name a
/// git repository or another registry.
fn patched_other_sources(root_manifest: &str) -> Vec<String> {
    let Ok(doc) = root_manifest.parse::<toml::Table>() else {
        // cargo already parsed it (the listing above); nothing to add.
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut entries = |table: Option<&toml::Value>, what: &str| {
        for (name, value) in table.and_then(toml::Value::as_table).into_iter().flatten() {
            if let Some(spec) = value.as_table() {
                for key in ["git", "registry", "registry-index"] {
                    if let Some(v) = spec.get(key) {
                        found.push(format!("{what} {name} from {key} {v}"));
                    }
                }
                if let Some(path) = spec.get("path").and_then(toml::Value::as_str)
                    && leaves(path)
                {
                    found.push(format!(
                        "{what} {name} from a path outside the project ({path})"
                    ));
                }
            }
        }
    };
    if let Some(patch) = doc.get("patch").and_then(toml::Value::as_table) {
        for (source, table) in patch {
            entries(Some(table), &format!("patch.{source}"));
        }
    }
    entries(doc.get("replace"), "replace");
    found
}

/// Whether a path relative to the root manifest leaves the project: absolute,
/// or climbing above the root with `..`.
fn leaves(rel: &str) -> bool {
    leaves_from(0, rel)
}

/// [`leaves`] for a path written in a manifest `depth` folders below the root.
fn leaves_from(depth: i32, rel: &str) -> bool {
    let path = Path::new(rel);
    if path.is_absolute() {
        return true;
    }
    let mut depth = depth;
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            _ => return true,
        }
        if depth < 0 {
            return true;
        }
    }
    false
}

/// The private copy of the project's manifests the commands run in.
struct Mirror {
    dir: tempfile::TempDir,
    /// What the manifests name that kGraf does not fetch (see
    /// [`manifest_findings`]).
    found: Findings,
}

/// What the mirrored manifests name that the fetch refuses.
#[derive(Debug, Default, PartialEq, Eq)]
struct Findings {
    /// Paths that leave the project. With any, cargo is not run on the
    /// mirror at all: a relative path that leaves it resolves against the
    /// mirror's parents, and cargo would read whatever manifest is there.
    escapes: Vec<String>,
    /// `git`, `registry` and `registry-index` keys: sources other than
    /// crates.io.
    sources: Vec<String>,
}

/// Where the mirrors of `checkout` are made: under its state directory, so
/// the folders above a mirror are the state directory and the checkout.
pub(crate) fn mirror_base(checkout: &Path) -> PathBuf {
    checkout
        .join(kgraf_core::legacy::STATE_DIR)
        .join(MIRROR_DIR)
}

impl Mirror {
    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn build(site: &FetchSite) -> Result<Self, DepsError> {
        let base = mirror_base(&site.checkout);
        std::fs::create_dir_all(&base)
            .map_err(|e| DepsError::io("cannot make the manifest mirror folder", e))?;
        remove_stale_mirrors(&base);
        let dir = tempfile::Builder::new()
            .prefix("kgraf-deps-")
            .tempdir_in(&base)
            .map_err(|e| DepsError::io("cannot make the manifest mirror", e))?;
        let source = Dir::open_ambient_dir(&site.source, ambient_authority())
            .map_err(|e| DepsError::io(format!("cannot open {}", site.source.display()), e))?;
        let root = dir.path().to_path_buf();
        let found = copy_manifests(&source, &root)?;
        if !root.join("Cargo.toml").is_file() {
            return Err(DepsError::Project {
                detail: format!("{} has no root Cargo.toml", site.source.display()),
            });
        }
        if let Some(lock) = read_regular(&source, Path::new("Cargo.lock"), MAX_LOCK_BYTES)? {
            write_new(&root.join("Cargo.lock"), &lock)?;
        }
        make_workspace_root(&root.join("Cargo.toml"))?;
        Ok(Self { dir, found })
    }
}

/// Make the mirrored root manifest a workspace root when it is not one and
/// does not name one. Measured: cargo reads each parent folder's `Cargo.toml`
/// as the workspace root of a manifest that has none (a sentinel parent with
/// `exclude` does not stop the climb), and the checkout's, one folder above
/// the state directory, would be read unmirrored and unchecked. An empty
/// `[workspace]` table ends the search at the mirror. A manifest that has a
/// `[workspace]` or a `package.workspace` is left as it is, so a real
/// workspace resolves as it does in the project.
fn make_workspace_root(manifest: &Path) -> Result<(), DepsError> {
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| DepsError::io("cannot read the mirrored root manifest", e))?;
    let Ok(doc) = text.parse::<toml::Table>() else {
        // Cargo refuses a manifest that does not parse, before any climb.
        return Ok(());
    };
    let names_workspace = doc.contains_key("workspace")
        || doc
            .get("package")
            .and_then(toml::Value::as_table)
            .is_some_and(|p| p.contains_key("workspace"));
    if names_workspace {
        return Ok(());
    }
    let mut text = text;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n[workspace]\n");
    std::fs::write(manifest, text).map_err(|e| DepsError::io("cannot write the manifest mirror", e))
}

/// Remove the mirrors under `base` older than [`STALE_MIRROR`]: those of a
/// process that died before its guard ran. Best effort; a failure leaves a
/// folder behind and changes nothing else. `remove_dir_all` does not follow
/// a link, and nothing but this module writes here.
fn remove_stale_mirrors(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_MIRROR);
        let ours = entry
            .file_name()
            .to_string_lossy()
            .starts_with("kgraf-deps-");
        if old && ours {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// When `root` has no `Cargo.toml`: the shallowest one below it, relative
/// to it, if any. Bounded in depth and entries, symlinks and hidden folders
/// skipped, like the mirror's walk.
fn find_nested_manifest(root: &Path) -> Option<PathBuf> {
    let dir = Dir::open_ambient_dir(root, ambient_authority()).ok()?;
    let mut queue = std::collections::VecDeque::from([(dir, PathBuf::new(), 0usize)]);
    let mut entries = 0usize;
    while let Some((dir, rel, depth)) = queue.pop_front() {
        for entry in dir.entries().ok()?.flatten() {
            entries += 1;
            if entries > NESTED_ENTRIES {
                return None;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() && name == "Cargo.toml" {
                return Some(rel.join(name));
            }
            if kind.is_dir()
                && !name.starts_with('.')
                && !SKIP_DIRS.contains(&name)
                && depth < NESTED_DEPTH
                && let Ok(sub) = entry.open_dir()
            {
                queue.push_back((sub, rel.join(name), depth + 1));
            }
        }
    }
    None
}

/// Copy every `Cargo.toml` beneath `source` to the same place under `to`,
/// each with an empty `src/lib.rs` stub. Iterative, bounded in depth and
/// entries; symlinks are skipped. Returns what the manifests name that the
/// fetch refuses.
fn copy_manifests(source: &Dir, to: &Path) -> Result<Findings, DepsError> {
    let mut found = Findings::default();
    let mut stack: Vec<(Dir, PathBuf, usize)> = vec![(
        source
            .try_clone()
            .map_err(|e| DepsError::io("cannot open the project", e))?,
        PathBuf::new(),
        0,
    )];
    let (mut manifests, mut entries) = (0usize, 0usize);
    while let Some((dir, rel, depth)) = stack.pop() {
        let listing = dir
            .entries()
            .map_err(|e| DepsError::io(format!("cannot list {}", rel.display()), e))?;
        for entry in listing {
            let entry =
                entry.map_err(|e| DepsError::io(format!("cannot list {}", rel.display()), e))?;
            entries += 1;
            if entries > MAX_ENTRIES {
                return Err(too_large(format!("more than {MAX_ENTRIES} entries")));
            }
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            // `file_type` does not follow a symlink: a link is neither.
            let kind = entry
                .file_type()
                .map_err(|e| DepsError::io(format!("cannot stat {name_str}"), e))?;
            if kind.is_dir() {
                if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str) {
                    continue;
                }
                if depth + 1 > MAX_DEPTH {
                    return Err(too_large(format!("folders deeper than {MAX_DEPTH}")));
                }
                let sub = entry
                    .open_dir()
                    .map_err(|e| DepsError::io(format!("cannot open {name_str}"), e))?;
                stack.push((sub, rel.join(name_str), depth + 1));
            } else if kind.is_file() && name_str == "Cargo.toml" {
                manifests += 1;
                if manifests > MAX_MANIFESTS {
                    return Err(too_large(format!(
                        "more than {MAX_MANIFESTS} Cargo.toml files"
                    )));
                }
                let Some(bytes) = read_regular(&dir, Path::new("Cargo.toml"), MAX_MANIFEST_BYTES)?
                else {
                    continue;
                };
                let one = manifest_findings(&rel, &String::from_utf8_lossy(&bytes));
                found.escapes.extend(one.escapes);
                found.sources.extend(one.sources);
                let target = to.join(&rel);
                std::fs::create_dir_all(target.join("src"))
                    .map_err(|e| DepsError::io("cannot write the manifest mirror", e))?;
                write_new(&target.join("Cargo.toml"), &bytes)?;
                write_new(&target.join("src").join("lib.rs"), b"")?;
                for stub in target_stubs(&String::from_utf8_lossy(&bytes)) {
                    // Never over a file the mirror already holds. A stub that
                    // cannot be written (its path runs through a file) leaves
                    // cargo to name the missing target itself.
                    let path = target.join(&stub);
                    if !path.exists() {
                        let _ = write_new(&path, b"");
                    }
                }
            }
        }
    }
    Ok(found)
}

/// The most target stubs one manifest may add to the mirror.
const MAX_TARGET_STUBS: usize = 512;

/// The source files `cargo metadata` needs to exist for the targets a
/// manifest names, relative to its folder: a library at a custom `path`, every
/// `[[bin]]`, `[[example]]`, `[[test]]` and `[[bench]]` (at its `path` or the
/// default place for its name), and the `default-run` binary. Without them
/// `cargo metadata` fails on a manifest whose targets have no file, and the
/// project reads as unchecked. A path that is absolute or has a `..`
/// component is skipped: no stub is written outside the manifest's folder.
fn target_stubs(manifest: &str) -> Vec<PathBuf> {
    let Ok(doc) = manifest.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    let mut add = |rel: &str| {
        let rel = Path::new(rel);
        let plain = rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if plain && !rel.as_os_str().is_empty() && out.len() < MAX_TARGET_STUBS {
            out.push(rel.to_path_buf());
        }
    };
    if let Some(path) = doc
        .get("lib")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("path"))
        .and_then(toml::Value::as_str)
    {
        add(path);
    }
    let mut bins: Vec<&str> = Vec::new();
    for (key, dir) in [
        ("bin", "src/bin"),
        ("example", "examples"),
        ("test", "tests"),
        ("bench", "benches"),
    ] {
        let targets = doc.get(key).and_then(toml::Value::as_array);
        for target in targets
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
        {
            let name = target.get("name").and_then(toml::Value::as_str);
            if key == "bin"
                && let Some(name) = name
            {
                bins.push(name);
            }
            match (target.get("path").and_then(toml::Value::as_str), name) {
                (Some(path), _) => add(path),
                (None, Some(name)) if is_target_name(name) => add(&format!("{dir}/{name}.rs")),
                _ => {}
            }
        }
    }
    let default_run = doc
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|p| p.get("default-run"))
        .and_then(toml::Value::as_str);
    if let Some(name) = default_run
        && !bins.contains(&name)
        && is_target_name(name)
    {
        add(&format!("src/bin/{name}.rs"));
    }
    out.sort();
    out.dedup();
    out
}

/// A target name that is safe as a file name: cargo's own rule for a package
/// or target name (letters, digits, `-`, `_`).
fn is_target_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Keys whose string values (or arrays of strings) are paths cargo resolves
/// from a manifest's folder: a dependency's or patch's `path`, a package's
/// `workspace`, and a workspace's member lists. `exclude` is one only in the
/// `[workspace]` table ([`is_path_key`]).
const PATH_KEYS: [&str; 4] = ["path", "workspace", "members", "default-members"];

/// Whether `key`, in the table at `at`, holds paths. `exclude` lists paths in
/// `[workspace]` only: `package.exclude` is a list of gitignore-style
/// patterns (`*.tmp`, `/target`, `../x`), never resolved as a path.
fn is_path_key(key: &str, at: &str) -> bool {
    PATH_KEYS.contains(&key) || (key == "exclude" && at == "workspace")
}

/// Keys that name a source other than crates.io: a git repository (any URL,
/// `file://` included) or another registry.
const SOURCE_KEYS: [&str; 3] = ["git", "registry", "registry-index"];

/// Where a table sits in a manifest, as far as it decides whether
/// [`SOURCE_KEYS`] mean a source. A key named `git` is a source only inside a
/// dependency specification; as a feature name, or as the name of a
/// dependency (`git = "0.3"`), it is not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Root,
    /// `[workspace]`.
    Workspace,
    /// `[target]`: each entry is one `cfg` or triple.
    Target,
    /// `[target.<cfg>]`.
    TargetCfg,
    /// `[patch]`: one table per source.
    Patch,
    /// A table whose entries are dependency specifications: `[dependencies]`,
    /// `[dev-dependencies]`, `[build-dependencies]`, `[workspace.dependencies]`,
    /// a target's, `[patch.<source>]` and `[replace]`.
    Deps,
    /// One dependency's specification, as a table or an inline table.
    Spec,
    Other,
}

impl Place {
    fn child(self, key: &str) -> Place {
        let deps = matches!(
            key,
            "dependencies"
                | "dev-dependencies"
                | "build-dependencies"
                | "dev_dependencies"
                | "build_dependencies"
        );
        match self {
            Place::Root => match key {
                _ if deps => Place::Deps,
                "workspace" => Place::Workspace,
                "target" => Place::Target,
                "patch" => Place::Patch,
                "replace" => Place::Deps,
                _ => Place::Other,
            },
            Place::Workspace if key == "dependencies" => Place::Deps,
            Place::Target => Place::TargetCfg,
            Place::TargetCfg if deps => Place::Deps,
            Place::Patch => Place::Deps,
            Place::Deps => Place::Spec,
            _ => Place::Other,
        }
    }
}

/// What the manifest at `rel` (its folder, relative to the project root)
/// names that the fetch refuses: paths that leave the project, and
/// [`SOURCE_KEYS`] inside a dependency specification. Conservative: every
/// path key anywhere but under the `package`, `project` and `workspace`
/// `metadata` tables counts, target paths included. A manifest that does not
/// parse is left to cargo, which refuses it.
fn manifest_findings(rel: &Path, manifest: &str) -> Findings {
    let mut found = Findings::default();
    let Ok(doc) = manifest.parse::<toml::Table>() else {
        return found;
    };
    let depth = rel
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count() as i32;
    let file = rel.join("Cargo.toml");
    let mut stack: Vec<(&toml::Table, String, Place)> = vec![(&doc, String::new(), Place::Root)];
    while let Some((table, at, place)) = stack.pop() {
        for (key, value) in table {
            let here = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            // Cargo ignores `metadata` only as `package.metadata`,
            // `project.metadata` and `workspace.metadata`. Anywhere else it
            // is a name (a renamed dependency, a target), so it is checked.
            if key == "metadata" && matches!(at.as_str(), "package" | "project" | "workspace") {
                continue;
            }
            // Any value at all: `git = true` is not valid, but cargo is the
            // judge of that, and refusing costs nothing.
            if place == Place::Spec && SOURCE_KEYS.contains(&key.as_str()) {
                found
                    .sources
                    .push(format!("{here} = {value} in {}", file.display()));
            }
            // `path` in `[lib]`, `[[bin]]`, `[[test]]`, `[[example]]` and
            // `[[bench]]` names a source file. Cargo does not open it to load
            // the manifest (the mirror holds no sources), so it is no path
            // that leaves the project.
            let target_source = key == "path"
                && matches!(at.as_str(), "lib" | "bin" | "test" | "example" | "bench");
            let path_key = is_path_key(key, &at) && !target_source;
            let strings: Vec<&str> = match value {
                toml::Value::String(v) if path_key => vec![v.as_str()],
                toml::Value::Array(items) if path_key => {
                    items.iter().filter_map(toml::Value::as_str).collect()
                }
                _ => Vec::new(),
            };
            for v in strings {
                if leaves_from(depth, v) {
                    found
                        .escapes
                        .push(format!("{here} = {v:?} in {}", file.display()));
                }
            }
            let inner = place.child(key);
            match value {
                toml::Value::Table(t) => stack.push((t, here, inner)),
                toml::Value::Array(items) => {
                    for t in items.iter().filter_map(toml::Value::as_table) {
                        stack.push((t, here.clone(), inner));
                    }
                }
                _ => {}
            }
        }
    }
    found
}

fn too_large(what: String) -> DepsError {
    DepsError::Project {
        detail: format!("the project is too large to mirror its manifests: {what}"),
    }
}

/// `rel` beneath `dir` when it is a regular file no larger than `cap`; `None`
/// when it is absent or not a regular file (a symlink, say).
fn read_regular(dir: &Dir, rel: &Path, cap: u64) -> Result<Option<Vec<u8>>, DepsError> {
    match dir.symlink_metadata(rel) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DepsError::io(format!("cannot stat {}", rel.display()), e)),
    }
    // Non-blocking: a file swapped for a named pipe after the check above
    // must not hold the fetch on `open`. The read below only happens for a
    // regular file (checked on the open descriptor).
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = dir
        .open_with(rel, &options)
        .map_err(|e| DepsError::io(format!("cannot open {}", rel.display()), e))?;
    let meta = file
        .metadata()
        .map_err(|e| DepsError::io(format!("cannot stat {}", rel.display()), e))?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > cap {
        return Err(too_large(format!("{} is over {cap} bytes", rel.display())));
    }
    let mut bytes = Vec::new();
    file.into_std()
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| DepsError::io(format!("cannot read {}", rel.display()), e))?;
    Ok(Some(bytes))
}

/// Write a file in the mirror (a directory this process made).
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), DepsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DepsError::io("cannot write the manifest mirror", e))?;
    }
    std::fs::write(path, bytes).map_err(|e| DepsError::io("cannot write the manifest mirror", e))
}

/// What else ends a command before its timeout.
#[derive(Clone, Copy, Default)]
struct Watch<'a> {
    /// Killed when this is set (the run ended, the call was cancelled).
    stop: Option<&'a Stop>,
    /// Killed once cargo reports more packages downloaded than this.
    max_downloads: Option<usize>,
}

/// Run `cmd` to its end, `timeout`, `watch`'s stop or its download bound,
/// whichever is first; killed at any but the end. Output is read on threads
/// so a full pipe never stalls cargo; stderr line by line, so downloads are
/// counted while they happen.
///
/// On Unix cargo leads its own process group, and a kill takes the whole
/// group: cargo can start children (a credential provider named in the
/// checkout's configuration, the `git` program), and none may outlive a
/// timeout or a cancel.
fn run_bounded(mut cmd: Command, timeout: Duration, watch: Watch<'_>) -> std::io::Result<Output> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(target_os = "linux")]
    super::group::die_with_parent(&mut cmd);
    let mut child = cmd.spawn()?;
    // The group dies with kGraf (see `group`). A registry that cannot take
    // the group, or handlers that cannot be installed, fail the command
    // rather than let it outlive an interrupt.
    #[cfg(unix)]
    let group = match super::group::GroupGuard::register(child.id()) {
        Ok(guard) => guard,
        Err(e) => {
            // SAFETY: killpg only signals the group this function created.
            unsafe {
                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(e);
        }
    };
    let stdout = child.stdout.take();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut p) = stdout {
            let _ = p.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let downloads = Arc::new(AtomicUsize::new(0));
    let stderr = child.stderr.take();
    let counted = downloads.clone();
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(p) = stderr {
            let mut reader = std::io::BufReader::new(p);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                let piece = String::from_utf8_lossy(&line);
                if is_package_download(piece.trim()) {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                text.push_str(&piece);
                line.clear();
            }
        }
        text
    });
    let deadline = Instant::now() + timeout;
    let kill = |child: &mut std::process::Child, kind, why: String| {
        // Before the wait below reaps the leader: its pid, the group's id, is
        // still reserved, so the signal cannot reach a reused group.
        #[cfg(unix)]
        {
            // SAFETY: killpg only signals the group this function created.
            unsafe {
                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
            }
            group.release();
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(std::io::Error::new(kind, why))
    };
    let status = loop {
        // Unix: look without reaping (`WNOWAIT`), so the leader's pid, the
        // group's id, stays reserved while the whole group is killed. Cargo
        // can leave a child behind that still holds the output pipes, and
        // the reads below would wait for it.
        #[cfg(unix)]
        if kgraf_sandbox::lifecycle::exited_unreaped(child.id())? {
            // SAFETY: killpg only signals the group this function created.
            unsafe {
                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
            }
            group.release();
            break child.wait()?;
        }
        #[cfg(not(unix))]
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return kill(
                &mut child,
                std::io::ErrorKind::TimedOut,
                format!("did not finish within {} s", timeout.as_secs()),
            );
        }
        if watch.stop.is_some_and(Stop::is_stopped) {
            return kill(
                &mut child,
                std::io::ErrorKind::Interrupted,
                "stopped: the run ended or the build that asked was cancelled".into(),
            );
        }
        let n = downloads.load(Ordering::SeqCst);
        if let Some(max) = watch.max_downloads
            && n > max
        {
            return kill(
                &mut child,
                std::io::ErrorKind::Other,
                format!("stopped after {n} packages, more than the {max} one fetch may download"),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(Output {
        ok: status.success(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Real cargo that cannot reach the network.
    fn offline_cargo() -> CargoDeps {
        CargoDeps {
            offline_only: true,
            ..CargoDeps::default()
        }
    }

    /// A git dependency refuses the fetch before `cargo fetch` runs, naming
    /// the source: the consent covers crates.io only. Real cargo, offline
    /// (`--no-deps` needs no network; the command runs with
    /// `CARGO_NET_OFFLINE`, so even a broken refusal cannot reach one).
    #[test]
    fn a_git_dependency_refuses_the_fetch_and_names_it() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "Cargo.toml",
            &format!(
                "{PKG}\n[dependencies]\nfrom-git = {{ git = \"https://example.invalid/g.git\" }}\n\
                 [workspace]\n"
            ),
        );
        write(root, "src/lib.rs", "");
        let err = offline_cargo()
            .fetch(&FetchSite::project(root), &Stop::default())
            .expect_err("refused");
        match err {
            DepsError::OtherSources { sources, .. } => {
                assert!(
                    sources
                        .iter()
                        .any(|s| s.contains("from-git") && s.contains("example.invalid")),
                    "{sources:?}"
                );
            }
            other => panic!("wrong refusal: {other}"),
        }
        assert!(
            !root.join("Cargo.lock").exists(),
            "nothing written in the project"
        );
    }

    /// A path dependency that leaves the project refuses the fetch: cargo
    /// would read a manifest on the host that is not the project's.
    #[test]
    fn a_path_dependency_outside_the_project_refuses_the_fetch() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join("p");
        write(
            &root,
            "Cargo.toml",
            &format!("{PKG}\n[dependencies]\nnear = {{ path = \"../near\" }}\n[workspace]\n"),
        );
        write(&root, "src/lib.rs", "");
        write(
            project.path(),
            "near/Cargo.toml",
            "[package]\nname = \"near\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(project.path(), "near/src/lib.rs", "");
        let err = offline_cargo()
            .fetch(&FetchSite::project(&root), &Stop::default())
            .expect_err("refused");
        assert!(
            matches!(&err, DepsError::OtherSources { sources, .. }
                if sources.iter().any(|s| s.contains("dependencies.near.path") && s.contains("../near"))),
            "{err}"
        );
        // The check does not run cargo on such a mirror either.
        assert!(matches!(offline_cargo().check(&FetchSite::project(&root)),
                Readiness::Unchecked(why) if why.contains("../near")),);
        // A path that stays inside is not flagged, from any depth.
        assert!(
            manifest_findings(
                Path::new("crates/a"),
                "[dependencies]\nb = { path = \"../b\" }\n[workspace]\nmembers = [\"x/*\"]\n"
            ) == Findings::default()
        );
        let found = manifest_findings(
            Path::new("crates/a"),
            "[package]\nworkspace = \"../../..\"\n[target.'cfg(unix)'.dependencies]\n\
             c = { path = \"/etc\" }\n[package.metadata.x]\npath = \"/ignored\"\n",
        );
        assert_eq!(found.escapes.len(), 2, "{found:?}");
    }

    /// `path` in a target table is a source file, not a manifest path: cargo
    /// does not open it, so one that climbs is no escape. A dependency's
    /// `path` still is.
    #[test]
    fn a_target_source_path_is_not_a_manifest_escape() {
        let found = manifest_findings(
            Path::new("crates/a"),
            "[lib]\npath = \"../../../x/lib.rs\"\n[[bin]]\nname = \"b\"\npath = \"/abs/main.rs\"\n\
             [[test]]\nname = \"t\"\npath = \"../../../t.rs\"\n",
        );
        assert_eq!(found, Findings::default(), "{found:?}");
        let dep = manifest_findings(
            Path::new("a"),
            "[lib]\npath = \"../x.rs\"\n[dependencies]\nz = { path = \"../../z\" }\n",
        );
        assert_eq!(dep.escapes.len(), 1, "{dep:?}");
    }

    /// A project too large to mirror, or with no root manifest, fails the
    /// same way every time: it is not a transient error to retry.
    #[test]
    fn a_project_that_cannot_be_mirrored_is_a_permanent_error() {
        assert!(!too_large("more than 1 entries".into()).is_transient());
        let project = tempfile::tempdir().unwrap();
        write(project.path(), "a/Cargo.toml", PKG);
        let err = Mirror::build(&FetchSite::project(project.path()))
            .err()
            .expect("no root manifest");
        assert!(
            matches!(err, DepsError::Project { .. }) && !err.is_transient(),
            "{err}"
        );
        // A real I/O failure is still transient.
        assert!(DepsError::io("x", std::io::Error::other("y")).is_transient());
    }

    /// The pid a shell child wrote to `file`, once it has (the command is
    /// running and has started its own child).
    #[cfg(unix)]
    fn pid_written(file: &Path) -> libc::pid_t {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(file)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "the command never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether `pid` is gone within a few seconds.
    #[cfg(unix)]
    fn gone(pid: libc::pid_t) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 only probes whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A shell that starts `sleep 30`, writes its pid to `$PID_FILE`, then
    /// `tail`.
    #[cfg(unix)]
    fn shell_with_sleeper(pid_file: &Path, tail: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 30 & echo $! > \"$PID_FILE\"; {tail}"))
            .env("PID_FILE", pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// A cancel kills the command's whole group, not only the leader: a
    /// child it started does not outlive the call. The test waits until the
    /// child exists, so it does not depend on how fast the shell starts.
    #[cfg(unix)]
    #[test]
    fn a_stop_kills_the_children_of_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let stop = Stop::default();
        let watch = Watch {
            stop: Some(&stop),
            max_downloads: None,
        };
        let err = std::thread::scope(|scope| {
            let run = scope.spawn(|| {
                run_bounded(
                    shell_with_sleeper(&pid_file, "wait"),
                    Duration::from_secs(120),
                    watch,
                )
            });
            let pid = pid_written(&pid_file);
            stop.stop();
            let err = run.join().unwrap().err().expect("stopped");
            assert!(gone(pid), "the child of the command is still running");
            err
        });
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    }

    /// A timeout ends the command, and promptly.
    #[cfg(unix)]
    #[test]
    fn a_timeout_ends_the_command() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = Instant::now();
        let err = run_bounded(cmd, Duration::from_millis(300), Watch::default())
            .err()
            .expect("timed out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    /// A command that exits and leaves a child holding its output pipes does
    /// not stall the call: the group is killed when the leader exits, before
    /// the pipes are read to their end.
    #[cfg(unix)]
    #[test]
    fn a_child_left_holding_the_pipes_does_not_stall_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let started = Instant::now();
        let out = run_bounded(
            shell_with_sleeper(&pid_file, "true"),
            Duration::from_secs(120),
            Watch::default(),
        )
        .expect("the command exits");
        assert!(out.ok);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the call waited for the child that held the pipes"
        );
        assert!(gone(pid_written(&pid_file)), "the child is still running");
    }

    /// Runs as the child of `a_sigint_kills_the_command_in_flight`; does
    /// nothing in a normal test run.
    #[cfg(unix)]
    #[test]
    fn signal_helper() {
        let Some(pid_file) = std::env::var_os("KGRAF_TEST_PID_FILE") else {
            return;
        };
        let _ = run_bounded(
            shell_with_sleeper(Path::new(&pid_file), "wait"),
            Duration::from_secs(120),
            Watch::default(),
        );
    }

    /// An interrupt of kGraf ends a command in flight. A second copy of this
    /// test binary runs a command, gets `SIGINT` (its default action stays:
    /// it dies of the signal), and the command's child must be gone.
    #[cfg(unix)]
    #[test]
    fn a_sigint_kills_the_command_in_flight() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deps::cargo::tests::signal_helper",
                "--test-threads",
                "1",
            ])
            .env("KGRAF_TEST_PID_FILE", &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let sleeper = pid_written(&pid_file);
        // SAFETY: signals the helper this test started.
        unsafe { libc::kill(helper.id() as libc::pid_t, libc::SIGINT) };
        let status = helper.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
        assert!(
            gone(sleeper),
            "the command outlived the interrupted process"
        );
    }

    /// Every `git`, `registry` and `registry-index` key in a dependency
    /// specification of any manifest (any dependency table, a target's, a
    /// patch, a replace), and none under `metadata`, is a source the fetch
    /// refuses.
    #[test]
    fn source_keys_are_found_in_every_dependency_table_but_metadata() {
        let found = manifest_findings(
            Path::new("vendor/x"),
            "[dependencies]\na = { git = \"file:///srv/repo\" }\n\
             [target.'cfg(unix)'.dev-dependencies]\nb = { version = \"1\", registry = \"corp\" }\n\
             [workspace.dependencies]\nc = { registry-index = \"https://example.invalid\" }\n\
             [patch.crates-io]\nd = { git = \"https://example.invalid/d\" }\n\
             [replace]\n\"e:1.0.0\" = { git = \"https://example.invalid/e\" }\n\
             [package.metadata.x]\ngit = \"ignored\"\n",
        );
        assert_eq!(found.sources.len(), 5, "{found:?}");
        assert!(found.escapes.is_empty());
        assert!(
            found.sources[0].contains("vendor/x/Cargo.toml"),
            "names the manifest: {found:?}"
        );
        assert!(
            found
                .sources
                .iter()
                .any(|s| s.contains("dependencies.a.git") && s.contains("file:///srv/repo")),
            "{found:?}"
        );
    }

    /// `package.exclude` is a list of gitignore-style patterns, not paths:
    /// `../x` or `/tmp` there escapes nothing. `workspace.exclude` is a list
    /// of paths and still counts.
    #[test]
    fn exclude_is_a_path_key_in_the_workspace_table_only() {
        let package = manifest_findings(
            Path::new("crates/a"),
            "[package]\nname = \"a\"\nexclude = [\"../x\", \"/tmp\", \"*.tmp\"]\n",
        );
        assert_eq!(package, Findings::default());
        let workspace = manifest_findings(
            Path::new("crates/a"),
            "[workspace]\nexclude = [\"../../../x\"]\n",
        );
        assert_eq!(workspace.escapes.len(), 1, "{workspace:?}");
        let inside = manifest_findings(Path::new("."), "[workspace]\nexclude = [\"old\"]\n");
        assert_eq!(inside, Findings::default());
    }

    /// A feature or a dependency that is merely named `git`, `registry` or
    /// `registry-index` is not a source; the same keys inside a dependency
    /// specification are.
    #[test]
    fn a_feature_or_a_dependency_named_git_is_not_a_source() {
        let named = manifest_findings(
            Path::new("."),
            "[features]\ngit = []\nregistry = [\"dep:x\"]\nregistry-index = []\n\
             [dependencies]\ngit = \"0.3\"\nregistry = { version = \"1\" }\n\
             registry-index = { version = \"2\", optional = true }\n\
             [dev-dependencies]\ngit = \"0.3\"\n\
             [target.'cfg(unix)'.dependencies]\ngit = \"0.3\"\n\
             [workspace.dependencies]\ngit = \"0.3\"\n",
        );
        assert!(named.sources.is_empty(), "{named:?}");
        // A dependency named `git` that does name a source is still found.
        for manifest in [
            "[dependencies]\ngit = { git = \"https://example.invalid/g\" }\n",
            "[dependencies.git]\ngit = \"https://example.invalid/g\"\n",
            "[dependencies]\nx = { registry = \"corp\" }\n",
            "[build-dependencies]\nx = { git = \"file:///r\" }\n",
            "[target.'cfg(unix)'.build-dependencies.x]\nregistry-index = \"https://e.invalid\"\n",
            "[patch.\"https://example.invalid/i\"]\nx = { git = \"file:///r\" }\n",
            "[workspace.dependencies]\nx = { git = \"file:///r\" }\n",
            "[replace]\n\"x:1.0.0\" = { git = \"file:///r\" }\n",
        ] {
            let found = manifest_findings(Path::new("."), manifest);
            assert_eq!(found.sources.len(), 1, "{manifest}: {found:?}");
        }
    }

    /// Cargo ignores `metadata` only as `package.metadata`,
    /// `project.metadata` and `workspace.metadata`. A dependency renamed
    /// `metadata`, or a target named `metadata`, is a dependency table.
    #[test]
    fn a_dependency_or_target_named_metadata_is_checked() {
        for key in ["git", "registry", "registry-index"] {
            for manifest in [
                format!(
                    "[dependencies]\nmetadata = {{ package = \"ryu\", version = \"1\", \
                     {key} = \"http://127.0.0.1:1/\" }}\n"
                ),
                format!(
                    "[dependencies.metadata]\npackage = \"ryu\"\nversion = \"1\"\n\
                     {key} = \"http://127.0.0.1:1/\"\n"
                ),
                format!(
                    "[target.metadata.dependencies]\nryu = {{ version = \"1\", \
                     {key} = \"http://127.0.0.1:1/\" }}\n"
                ),
                format!(
                    "[target.'cfg(unix)'.build-dependencies]\nmetadata = {{ package = \"ryu\", \
                     version = \"1\", {key} = \"http://127.0.0.1:1/\" }}\n"
                ),
            ] {
                let found = manifest_findings(Path::new("vendor/v"), &manifest);
                assert_eq!(found.sources.len(), 1, "{key}: {manifest}: {found:?}");
            }
            // The three tables cargo ignores stay ignored.
            for manifest in [
                format!("[package.metadata.x]\n{key} = \"ignored\"\n"),
                format!("[workspace.metadata]\n{key} = \"ignored\"\n"),
                format!("[project.metadata.x.metadata]\n{key} = \"ignored\"\n"),
            ] {
                let found = manifest_findings(Path::new("."), &manifest);
                assert!(found.sources.is_empty(), "{manifest}: {found:?}");
            }
        }
    }

    /// The renamed `metadata` dependency of the report, in a manifest cargo
    /// reaches but `cargo metadata --no-deps` does not list (a path
    /// dependency into an excluded folder), and in a workspace member.
    #[test]
    fn a_renamed_metadata_dependency_refuses_the_fetch_in_an_excluded_folder_and_a_member() {
        for key in ["git", "registry", "registry-index"] {
            let bad = format!(
                "[package]\nname = \"v\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 [dependencies]\nmetadata = {{ package = \"ryu\", version = \"1\", \
                 {key} = \"http://127.0.0.1:1/\" }}\n\
                 [target.metadata.dependencies]\nitoa = {{ version = \"1\", \
                 {key} = \"http://127.0.0.1:2/\" }}\n"
            );
            // Excluded folder, reached by a path dependency.
            let project = tempfile::tempdir().unwrap();
            let root = project.path();
            write(
                root,
                "Cargo.toml",
                &format!(
                    "{PKG}[dependencies]\nv = {{ path = \"vendor/v\" }}\n[workspace]\n\
                     exclude = [\"vendor\"]\n"
                ),
            );
            write(root, "src/lib.rs", "");
            write(root, "vendor/v/Cargo.toml", &bad);
            let sources = refused_sources(root);
            assert!(
                sources.iter().any(|s| s.contains("metadata")
                    && s.contains(key)
                    && s.contains("vendor/v/Cargo.toml")),
                "{key}: {sources:?}"
            );
            // A workspace member.
            let project = tempfile::tempdir().unwrap();
            let root = project.path();
            write(
                root,
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/v\"]\n",
            );
            write(root, "crates/v/src/lib.rs", "");
            write(root, "crates/v/Cargo.toml", &bad);
            let sources = refused_sources(root);
            assert!(
                sources
                    .iter()
                    .any(|s| s.contains("metadata") && s.contains(key)),
                "{key}: {sources:?}"
            );
        }
    }

    /// A root manifest is left alone when it is a workspace or names one, and
    /// gets an empty `[workspace]` otherwise (also without a final newline).
    #[test]
    fn only_a_root_manifest_with_no_workspace_is_made_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Cargo.toml");
        for (text, changed) in [
            ("[workspace]\nmembers = []\n", false),
            ("[package]\nname = \"a\"\nworkspace = \"ws\"\n", false),
            ("workspace.members = []\n", false),
            ("[package]\nname = \"a\"", true),
            ("[package]\nname = \"a\"\n[dependencies]\n", true),
            ("[[bin]]\nname = \"a\"\n", true),
        ] {
            std::fs::write(&file, text).unwrap();
            make_workspace_root(&file).unwrap();
            let after = std::fs::read_to_string(&file).unwrap();
            if changed {
                assert!(after.starts_with(text) && after.ends_with("\n[workspace]\n"));
                assert!(
                    after
                        .parse::<toml::Table>()
                        .unwrap()
                        .contains_key("workspace")
                );
            } else {
                assert_eq!(after, text);
            }
        }
    }

    /// A project root manifest with no `[workspace]`: cargo would climb from
    /// the mirror to the checkout's own `Cargo.toml`, which is not mirrored.
    /// Every mirror is its own workspace root, so a `[patch]` there is never
    /// read, for a single package and for a real workspace alike.
    #[test]
    fn a_checkout_root_manifest_above_the_mirror_is_never_read() {
        // A single package with no `[workspace]`: the checkout's manifest is
        // the project's own root, so the mirror copy must not climb above
        // itself. The state directory sits under the checkout; plant a
        // second `Cargo.toml` above the mirror with a git patch, as the
        // checkout root is when the source is a worktree elsewhere.
        for workspace in [false, true] {
            let checkout = tempfile::tempdir().unwrap();
            let worktree = tempfile::tempdir().unwrap();
            write(
                checkout.path(),
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 [dependencies]\nitoa = \"1\"\n[workspace]\n\
                 [patch.crates-io]\nitoa = { git = \"http://127.0.0.1:1/itoa.git\" }\n",
            );
            write(checkout.path(), "src/lib.rs", "");
            if workspace {
                write(
                    worktree.path(),
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"a\"]\n",
                );
                write(worktree.path(), "a/Cargo.toml", PKG);
                write(worktree.path(), "a/src/lib.rs", "");
            } else {
                write(worktree.path(), "Cargo.toml", PKG);
                write(worktree.path(), "src/lib.rs", "");
            }
            let site = FetchSite {
                source: worktree.path().to_path_buf(),
                checkout: checkout.path().to_path_buf(),
            };
            let mirror = Mirror::build(&site).unwrap();
            let out = offline_cargo()
                .run(
                    mirror.path(),
                    &["metadata", "--offline", "--format-version", "1"],
                    Duration::from_secs(60),
                    Watch::default(),
                )
                .unwrap();
            assert!(out.ok, "workspace={workspace}: {}", out.stderr);
            assert!(
                !out.stdout.contains("outer") && !out.stdout.contains("127.0.0.1"),
                "the checkout's manifest was read (workspace={workspace}): {}",
                out.stdout
            );
            // The copy of a package with no workspace gets an empty one; a
            // real workspace root is copied as it is.
            let copied = std::fs::read_to_string(mirror.path().join("Cargo.toml")).unwrap();
            assert!(copied.contains("[workspace]"), "{copied}");
            if workspace {
                assert_eq!(copied, "[workspace]\nmembers = [\"a\"]\n");
            } else {
                assert!(copied.starts_with(PKG), "{copied}");
            }
            // A real workspace still resolves as one.
            assert!(
                out.stdout.contains("\"name\":\"a\""),
                "workspace={workspace}: {}",
                out.stdout
            );
        }
    }

    /// The fetch's refusal for a project, with real cargo offline.
    fn refused_sources(root: &Path) -> Vec<String> {
        match offline_cargo().fetch(&FetchSite::project(root), &Stop::default()) {
            Err(DepsError::OtherSources { sources, .. }) => sources,
            other => panic!("not refused as another source: {other:?}"),
        }
    }

    /// A root `[patch.crates-io]` path into an excluded vendor folder whose
    /// manifest names a git dependency: not a member, so `cargo metadata
    /// --no-deps` does not list it, but cargo would fetch it.
    #[test]
    fn a_git_dependency_behind_a_patch_into_an_excluded_folder_is_refused() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "Cargo.toml",
            &format!(
                "{PKG}[dependencies]\nitoa = \"1\"\n[workspace]\nexclude = [\"vendor\"]\n\
                 [patch.crates-io]\nitoa = {{ path = \"vendor/itoa\" }}\n"
            ),
        );
        write(root, "src/lib.rs", "");
        write(
            root,
            "vendor/itoa/Cargo.toml",
            "[package]\nname = \"itoa\"\nversion = \"1.0.99\"\nedition = \"2021\"\n\
             [dependencies]\nevil = { git = \"https://attacker.example/evil.git\" }\n",
        );
        let sources = refused_sources(root);
        assert!(
            sources
                .iter()
                .any(|s| s.contains("attacker.example/evil.git")
                    && s.contains("vendor/itoa/Cargo.toml")),
            "{sources:?}"
        );
    }

    /// A root package whose `package.workspace` names a folder whose
    /// manifest makes it a member and patches crates.io from git.
    #[test]
    fn a_git_patch_in_a_workspace_root_named_by_the_package_is_refused() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             workspace = \"ws\"\n[dependencies]\nitoa = \"1\"\n",
        );
        write(root, "src/lib.rs", "");
        write(
            root,
            "ws/Cargo.toml",
            "[workspace]\nmembers = [\"..\"]\n[patch.crates-io]\n\
             itoa = { git = \"https://attacker.example/itoa.git\" }\n",
        );
        let sources = refused_sources(root);
        assert!(
            sources
                .iter()
                .any(|s| s.contains("attacker.example/itoa.git") && s.contains("ws/Cargo.toml")),
            "{sources:?}"
        );
    }

    /// A member's path dependency into an excluded folder whose manifest
    /// names a git dependency.
    #[test]
    fn a_git_dependency_of_a_path_dependency_in_an_excluded_folder_is_refused() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "Cargo.toml",
            &format!(
                "{PKG}[dependencies]\nv = {{ path = \"vendor/v\" }}\n[workspace]\n\
                 exclude = [\"vendor\"]\n"
            ),
        );
        write(root, "src/lib.rs", "");
        write(
            root,
            "vendor/v/Cargo.toml",
            "[package]\nname = \"v\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nevil = { git = \"https://attacker.example/evil3.git\" }\n",
        );
        let sources = refused_sources(root);
        assert!(
            sources
                .iter()
                .any(|s| s.contains("attacker.example/evil3.git")),
            "{sources:?}"
        );
    }

    /// A `file://` git source (a local repository cargo would copy into the
    /// cargo home, which workers can read) is refused like any git source.
    #[test]
    fn a_file_url_git_source_is_refused() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let secret = project.path().join("secret-repo");
        write(
            root,
            "Cargo.toml",
            &format!(
                "{PKG}[dependencies]\nsecret = {{ git = \"file://{}\" }}\n[workspace]\n",
                secret.display()
            ),
        );
        write(root, "src/lib.rs", "");
        let sources = refused_sources(root);
        assert!(sources.iter().any(|s| s.contains("file://")), "{sources:?}");
        // The same source in a path dependency's manifest, in a folder the
        // workspace excludes: not a member, so only the manifest reading
        // sees it.
        write(
            root,
            "Cargo.toml",
            &format!(
                "{PKG}[dependencies]\nv = {{ path = \"vendor/v\" }}\n[workspace]\n\
                 exclude = [\"vendor\"]\n"
            ),
        );
        write(
            root,
            "vendor/v/Cargo.toml",
            &format!(
                "[package]\nname = \"v\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 [dependencies]\nsecret = {{ git = \"file://{}\" }}\n",
                secret.display()
            ),
        );
        let sources = refused_sources(root);
        assert!(
            sources
                .iter()
                .any(|s| s.contains("file://") && s.contains("vendor/v/Cargo.toml")),
            "{sources:?}"
        );
    }

    /// Every cargo command runs with git fetched by the `git` program and
    /// every git transport refused.
    #[test]
    fn every_command_refuses_git_transports() {
        let cmd = CargoDeps::default().command(Path::new("."), &["fetch"]);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["--config", "net.git-fetch-with-cli=true", "fetch"],
            "{args:?}"
        );
        let allow = cmd
            .get_envs()
            .find(|(k, _)| *k == "GIT_ALLOW_PROTOCOL")
            .and_then(|(_, v)| v);
        assert_eq!(allow, Some(std::ffi::OsStr::new("none")));
    }

    /// The mechanism the variable relies on: `git` itself refuses a clone
    /// over `file://` (and a plain local path, which is the same transport)
    /// when `GIT_ALLOW_PROTOCOL=none`, and allows it otherwise. Local only:
    /// a repository in a temporary folder, no network.
    #[test]
    fn git_refuses_every_transport_with_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = |args: &[&str], cwd: &Path, allow: Option<&str>| {
            let mut c = Command::new("git");
            c.args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null());
            match allow {
                Some(v) => c.env("GIT_ALLOW_PROTOCOL", v),
                None => c.env_remove("GIT_ALLOW_PROTOCOL"),
            };
            c.output().expect("git runs")
        };
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git(&["init", "-q"], &repo, None).status.success());
        std::fs::write(repo.join("f"), "x").unwrap();
        assert!(git(&["add", "f"], &repo, None).status.success());
        assert!(
            git(
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "m"
                ],
                &repo,
                None
            )
            .status
            .success()
        );
        let url = format!("file://{}", repo.display());
        let local = repo.display().to_string();
        for (source, dest) in [(url.as_str(), "c1"), (local.as_str(), "c2")] {
            let refused = git(&["clone", "-q", source, dest], dir.path(), Some("none"));
            assert!(!refused.status.success(), "{source} was cloned");
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains("not allowed"),
                "{}",
                String::from_utf8_lossy(&refused.stderr)
            );
            assert!(!dir.path().join(dest).join("f").exists());
        }
        let control = git(&["clone", "-q", &url, "c3"], dir.path(), None);
        assert!(control.status.success(), "the control clone works");
    }

    const PKG: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

    #[test]
    fn the_mirror_holds_manifests_and_stubs_only_under_the_state_directory() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(root, "Cargo.lock", "# lock\n");
        write(root, "crates/a/Cargo.toml", PKG);
        write(root, "crates/a/src/main.rs", "fn main() {}\n");
        write(root, "target/x/Cargo.toml", PKG);
        write(root, ".hidden/Cargo.toml", PKG);
        write(root, ".cargo/config.toml", "[net]\nretry = 1\n");
        write(root, "secret.txt", "not copied");
        let mirror = Mirror::build(&FetchSite::project(root)).unwrap();
        let m = mirror.path();
        assert!(m.join("Cargo.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(m.join("Cargo.lock")).unwrap(),
            "# lock\n"
        );
        assert!(m.join("crates/a/Cargo.toml").is_file());
        assert_eq!(std::fs::read(m.join("crates/a/src/lib.rs")).unwrap(), b"");
        assert!(
            !m.join("crates/a/src/main.rs").exists(),
            "sources are not copied"
        );
        assert!(!m.join("target").exists());
        assert!(!m.join(".hidden").exists());
        assert!(!m.join("secret.txt").exists());
        assert!(
            !m.join(".cargo").exists(),
            "no settings are copied: cargo finds the checkout's by walking up"
        );
        assert!(
            m.starts_with(root.join(".kgraf").join(MIRROR_DIR)),
            "the mirror is under the checkout's state directory: {}",
            m.display()
        );
        let kept = m.to_path_buf();
        drop(mirror);
        assert!(!kept.exists(), "the mirror is removed with its guard");
    }

    /// A worker's worktree: its own cargo settings are never carried, only
    /// the checkout's; a symlinked manifest or folder is skipped, so nothing
    /// outside the worktree is read through it.
    #[cfg(unix)]
    #[test]
    fn a_worktree_mirror_skips_links_and_the_worktrees_own_settings() {
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "Cargo.toml",
            "[package]\nname = \"stolen\"\n",
        );
        write(
            outside.path(),
            "sub/Cargo.toml",
            "[package]\nname = \"stolen2\"\n",
        );
        let checkout = tempfile::tempdir().unwrap();
        write(
            checkout.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"stable\"\n",
        );
        let wt = tempfile::tempdir().unwrap();
        write(wt.path(), "Cargo.toml", PKG);
        write(
            wt.path(),
            ".cargo/config.toml",
            "[registries]\nevil = { index = \"x\" }\n",
        );
        write(
            wt.path(),
            "rust-toolchain.toml",
            "[toolchain]\npath = \"/tmp/evil\"\n",
        );
        std::os::unix::fs::symlink(outside.path().join("Cargo.toml"), wt.path().join("b.toml"))
            .unwrap();
        std::fs::create_dir_all(wt.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("sub"), wt.path().join("linkdir")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("Cargo.toml"),
            wt.path().join("linked/Cargo.toml"),
        )
        .unwrap();
        let site = FetchSite {
            source: wt.path().to_path_buf(),
            checkout: checkout.path().to_path_buf(),
        };
        let mirror = Mirror::build(&site).unwrap();
        let m = mirror.path();
        // The root manifest as written, plus the empty workspace that makes
        // the mirror its own root.
        assert_eq!(
            std::fs::read_to_string(m.join("Cargo.toml")).unwrap(),
            format!("{PKG}\n[workspace]\n")
        );
        assert!(
            !m.join("linked/Cargo.toml").exists(),
            "a linked manifest is skipped"
        );
        assert!(
            !m.join("linkdir").exists(),
            "a linked folder is not entered"
        );
        assert!(
            !m.join(".cargo").exists(),
            "the worktree's cargo settings are not carried"
        );
        assert!(
            !m.join("rust-toolchain.toml").exists(),
            "the worktree's toolchain file is not carried"
        );
        assert!(
            m.ancestors().any(|a| a == checkout.path()),
            "the checkout's toolchain file applies from a parent folder"
        );
    }

    /// A shell script standing in for cargo: answers `metadata` with an
    /// empty package list, and runs `body` for anything else.
    #[cfg(unix)]
    fn fake_cargo(dir: &Path, body: &str) -> CargoDeps {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-cargo.sh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *metadata*) echo '{{\"packages\":[]}}'; exit 0;;\n\
                 esac\n{body}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        CargoDeps {
            cargo: path,
            offline_only: true,
            ..CargoDeps::default()
        }
    }

    /// The host's cargo never lets rustup download a toolchain (the consent
    /// covers crates). A script stands in for the rustup shim: with
    /// `RUSTUP_AUTO_INSTALL=0` it refuses as rustup does; without it, it
    /// "downloads" (writes a marker). Both the check and the fetch must meet
    /// the refusal, and the marker must never appear.
    #[cfg(unix)]
    #[test]
    fn rustup_never_downloads_a_toolchain_for_the_check_or_the_fetch() {
        use std::os::unix::fs::PermissionsExt;
        let tools = tempfile::tempdir().unwrap();
        let marker = tools.path().join("DOWNLOADED");
        let path = tools.path().join("shim.sh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$RUSTUP_AUTO_INSTALL\" = 0 ]; then\n  echo \"error: toolchain \
                 '1.2.3-x' is not installed\" >&2\n  exit 1\nfi\necho downloaded >> '{}'\n\
                 echo '{{\"packages\":[]}}'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cargo = CargoDeps {
            cargo: path,
            offline_only: true,
            ..CargoDeps::default()
        };
        let project = small_project();
        let site = FetchSite::project(project.path());
        assert!(matches!(cargo.check(&site), Readiness::Unchecked(why)
                if why.contains("never downloads") && why.contains("rustup toolchain install")),);
        let err = cargo.fetch(&site, &Stop::default()).expect_err("refused");
        assert!(err.to_string().contains("is not installed"), "{err}");
        assert!(!marker.exists(), "the shim was allowed to download");
    }

    /// A manifest with a binary that has no path, a `default-run`, an example,
    /// a bench and a library at a custom path checks as decided: the mirror
    /// holds a stub for each target, so `cargo metadata` does not fail on a
    /// missing file and leave the project unchecked.
    #[test]
    fn the_mirror_holds_a_stub_for_every_named_target() {
        let project = tempfile::tempdir().unwrap();
        write(
            project.path(),
            "Cargo.toml",
            &format!(
                "{PKG}default-run = \"tool\"\n\
                 [lib]\npath = \"core/lib.rs\"\n\
                 [[bin]]\nname = \"tool\"\n\
                 [[bin]]\nname = \"other\"\npath = \"cmd/other.rs\"\n\
                 [[example]]\nname = \"ex\"\n\
                 [[bench]]\nname = \"b\"\nharness = false\n\
                 [workspace]\n"
            ),
        );
        let site = FetchSite::project(project.path());
        let mirror = Mirror::build(&site).unwrap();
        for stub in [
            "core/lib.rs",
            "src/bin/tool.rs",
            "cmd/other.rs",
            "examples/ex.rs",
            "benches/b.rs",
        ] {
            assert!(mirror.path().join(stub).is_file(), "no stub {stub}");
        }
        assert!(
            matches!(offline_cargo().check(&site), Readiness::Ready),
            "a project whose targets the mirror lacks is unchecked"
        );
    }

    /// A target path that leaves the manifest's folder gets no stub.
    #[test]
    fn a_target_path_that_leaves_the_folder_gets_no_stub() {
        let stubs = target_stubs(
            "[package]\nname = \"a\"\n[[bin]]\nname = \"x\"\npath = \"../x.rs\"\n\
             [[bin]]\nname = \"y\"\npath = \"/tmp/y.rs\"\n\
             [[bin]]\nname = \"a/../../z\"\n",
        );
        assert!(stubs.is_empty(), "{stubs:?}");
    }

    #[cfg(unix)]
    fn small_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        write(project.path(), "Cargo.toml", &format!("{PKG}[workspace]\n"));
        write(project.path(), "src/lib.rs", "");
        project
    }

    /// A fetch that reports more packages than the bound is killed, and
    /// says so.
    #[cfg(unix)]
    #[test]
    fn a_fetch_past_the_download_bound_is_killed() {
        let tools = tempfile::tempdir().unwrap();
        let mut cargo = fake_cargo(
            tools.path(),
            "for p in a b c d e; do echo \"  Downloaded $p v1.0.0\" >&2; done\nexec sleep 30",
        );
        cargo.max_downloads = 3;
        let project = small_project();
        let started = Instant::now();
        let err = cargo
            .fetch(&FetchSite::project(project.path()), &Stop::default())
            .expect_err("stopped");
        assert!(
            err.to_string()
                .contains("more than the 3 one fetch may download"),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed, not waited out"
        );
        // Under the bound, the report counts each package.
        let fine = fake_cargo(
            tools.path(),
            "echo '    Updating crates.io index' >&2\n\
             for p in a b; do echo \"  Downloaded $p v1.0.0\" >&2; done",
        );
        let report = fine
            .fetch(&FetchSite::project(project.path()), &Stop::default())
            .unwrap();
        assert_eq!(report.downloaded, 2);
    }

    /// A fetch running when its stop is set is killed at once.
    #[cfg(unix)]
    #[test]
    fn a_stopped_fetch_is_killed() {
        let tools = tempfile::tempdir().unwrap();
        let cargo = fake_cargo(tools.path(), "exec sleep 30");
        let project = small_project();
        let stop = Stop::default();
        let setter = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                stop.stop();
            })
        };
        let started = Instant::now();
        let err = cargo
            .fetch(&FetchSite::project(project.path()), &stop.child())
            .expect_err("stopped");
        setter.join().unwrap();
        assert!(err.to_string().contains("stopped"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed, not waited out"
        );
    }

    /// cargo's words name the mirror by a placeholder, never by its path.
    #[cfg(unix)]
    #[test]
    fn the_mirror_path_never_reaches_a_message() {
        let tools = tempfile::tempdir().unwrap();
        let cargo = fake_cargo(
            tools.path(),
            "echo \"error: failed to read $(pwd -P)/Cargo.toml and $PWD/x\" >&2\nexit 101",
        );
        let project = small_project();
        let err = cargo
            .fetch(&FetchSite::project(project.path()), &Stop::default())
            .expect_err("failed");
        let text = err.to_string();
        assert!(text.contains("<mirror>/Cargo.toml"), "{text}");
        assert!(!text.contains("kgraf-deps-"), "{text}");
    }

    /// The mirror's parent folders are the state directory and the checkout,
    /// so the checkout's own cargo configuration applies (here a
    /// `build.rustc-wrapper` that leaves a mark when `cargo metadata` asks
    /// rustc for its version).
    #[cfg(unix)]
    #[test]
    fn the_checkouts_own_cargo_configuration_applies() {
        use std::os::unix::fs::PermissionsExt;
        let project = small_project();
        let root = project.path();
        let marker = root.join("MARKER");
        let wrapper = root.join("wrap.sh");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho ran >> '{}'\nexec \"$@\"\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        write(
            root,
            ".cargo/config.toml",
            &format!("[build]\nrustc-wrapper = '{}'\n", wrapper.display()),
        );
        assert_eq!(
            offline_cargo().check(&FetchSite::project(root)),
            Readiness::Ready
        );
        assert!(marker.exists(), "the checkout's configuration was not read");
    }

    /// The environment variable naming the checkout of
    /// [`planted_temp_parent_helper`].
    const PLANTED_CHECKOUT_VAR: &str = "KGRAF_TEST_DEPS_PLANTED_CHECKOUT";

    /// A cargo configuration planted in the system temporary folder (a
    /// `build.rustc-wrapper`) has no effect on the host's cargo. The check
    /// runs in a child test process whose `TMPDIR` is a folder holding the
    /// planted file, so a mirror made in the system temporary folder would
    /// have it as a parent; the checkout is elsewhere.
    #[cfg(unix)]
    #[test]
    fn a_file_planted_above_the_system_temporary_folder_has_no_effect() {
        use std::os::unix::fs::PermissionsExt;
        let planted = tempfile::tempdir().unwrap();
        let marker = planted.path().join("MARKER");
        let wrapper = planted.path().join("wrap.sh");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho ran >> '{}'\nexec \"$@\"\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        write(
            planted.path(),
            ".cargo/config.toml",
            &format!("[build]\nrustc-wrapper = '{}'\n", wrapper.display()),
        );
        write(
            planted.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"kgraf-test-no-such-toolchain\"\n",
        );
        let project = small_project();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deps::cargo::tests::planted_temp_parent_helper",
                "--ignored",
                "--test-threads=1",
            ])
            .env("TMPDIR", planted.path())
            .env(PLANTED_CHECKOUT_VAR, project.path())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the helper ran: {text}");
        assert!(!marker.exists(), "the planted configuration was read");
    }

    /// Run only by the test above, in a child process.
    #[test]
    #[ignore = "run in a child process by a_file_planted_above_the_system_temporary_folder_has_no_effect"]
    fn planted_temp_parent_helper() {
        let Some(checkout) = std::env::var_os(PLANTED_CHECKOUT_VAR) else {
            return;
        };
        let site = FetchSite::project(Path::new(&checkout));
        assert_eq!(offline_cargo().check(&site), Readiness::Ready);
    }

    /// No root manifest but a nested one: the gate says the project was not
    /// checked; with none at all, nothing applies.
    #[test]
    fn a_nested_manifest_without_a_root_one_is_said() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let store = super::super::ConsentStore::in_state_dir(root);
        let run = |root: &Path| {
            super::super::ensure(
                &offline_cargo(),
                &FetchSite::project(root),
                &store,
                super::super::RunFetch::Stored,
                super::super::Trigger::RunStart,
            )
            .unwrap()
        };
        assert_eq!(run(root), super::super::GateOutcome::NotApplicable);
        write(root, "tools/x/Cargo.toml", PKG);
        write(root, ".hidden/Cargo.toml", PKG);
        match run(root) {
            super::super::GateOutcome::Unchecked(why) => {
                assert!(why.contains("tools/x/Cargo.toml"), "{why}");
                assert!(why.contains("cargo fetch"), "{why}");
            }
            other => panic!("not said: {other:?}"),
        }
    }

    #[test]
    fn a_missing_package_is_missing_and_other_failures_are_unchecked() {
        let missing = Output {
            ok: false,
            stdout: String::new(),
            stderr: "error: no matching package named `zzz` found\nlocation searched: crates.io \
                     index\nAs a reminder, you're using offline mode (--offline)"
                .into(),
        };
        assert!(matches!(classify(&missing), Readiness::Missing(d) if d.contains("zzz")));
        let broken = Output {
            ok: false,
            stdout: String::new(),
            stderr: "error: failed to parse manifest at `Cargo.toml`".into(),
        };
        assert!(matches!(classify(&broken), Readiness::Unchecked(_)));
        let fine = Output {
            ok: true,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(classify(&fine), Readiness::Ready);
    }

    #[test]
    fn the_report_names_each_index_contacted_and_counts_packages() {
        let stderr = "    Updating crates.io index\n  Downloaded itoa v1.0.18\n  Downloaded \
                      ryu v1.0.20\n  Downloaded 2 crates (80.1 KB) in 0.31s\n";
        let r = report_of(stderr, Duration::from_millis(400));
        assert_eq!(r.contacted, ["Updating crates.io index"]);
        assert_eq!(r.downloaded, 2);
        assert!(
            r.summary()
                .contains("Updating crates.io index; 2 package(s)")
        );
        // Every host cargo names is reported, not only registry indexes.
        let other = "    Updating crates.io index\n    Updating git repository \
                     `https://example.invalid/g`\n    Updating `corp` index\n";
        assert_eq!(
            report_of(other, Duration::ZERO).contacted,
            [
                "Updating crates.io index",
                "Updating git repository `https://example.invalid/g`",
                "Updating `corp` index"
            ]
        );
    }

    #[test]
    fn git_and_other_registry_sources_are_found() {
        let metadata = r#"{"packages":[{"dependencies":[
            {"name":"serde","source":"registry+https://github.com/rust-lang/crates.io-index","registry":null},
            {"name":"local","source":null,"registry":null,"path":"/x"},
            {"name":"inside","source":null,"registry":null,"path":"/m/crates/inside"},
            {"name":"g","source":"git+https://example.com/g.git","registry":null},
            {"name":"p","source":"registry+https://other.example/index","registry":"https://other.example/index"}
        ]}]}"#;
        let found = declared_other_sources(metadata, &[PathBuf::from("/m")]).unwrap();
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(
            found
                .iter()
                .any(|f| f.contains("local from a path outside")),
            "{found:?}"
        );
        assert!(found.iter().any(|f| f.starts_with("g from git+")));
        assert!(!found.iter().any(|f| f.starts_with("serde")));
        let patched = patched_other_sources(
            "[patch.crates-io]\nfoo = { git = \"https://example.com/foo\" }\nbar = { path = \
             \"bar\" }\n[replace]\n\"baz:1.0.0\" = { git = \"https://example.com/baz\" }\n",
        );
        assert_eq!(patched.len(), 2, "{patched:?}");
        assert!(patched[0].contains("foo"));
        let escaping = patched_other_sources(
            "[patch.crates-io]\na = { path = \"../../elsewhere\" }\nb = { path = \"/abs\" }\n\
             c = { path = \"vendor/../c\" }\n",
        );
        assert_eq!(escaping.len(), 2, "{escaping:?}");
        assert!(leaves("a/../../b") && !leaves("a/../b") && !leaves("./a"));
    }

    /// cargo itself, offline, on a project with no registry dependency: the
    /// check says ready, and a registry dependency that is not cached is
    /// missing. No network: `--offline`, and nothing is fetched.
    #[test]
    fn the_real_offline_check_tells_ready_from_missing() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(root, "Cargo.toml", &format!("{PKG}\n[workspace]\n"));
        write(root, "src/lib.rs", "");
        let site = FetchSite::project(root);
        let cargo = offline_cargo();
        assert!(cargo.applies(&site));
        assert_eq!(cargo.check(&site), Readiness::Ready);
        assert!(
            !root.join("Cargo.lock").exists(),
            "no lockfile written in the project"
        );
        write(
            root,
            "Cargo.toml",
            &format!(
                "{PKG}\n[dependencies]\nkgraf-test-never-published-zz = \"0.1\"\n[workspace]\n"
            ),
        );
        assert!(
            matches!(cargo.check(&site), Readiness::Missing(d) if d.contains("kgraf-test-never-published-zz")),
        );
        assert!(!cargo.applies(&FetchSite::project(&root.join("src"))));
    }
}
