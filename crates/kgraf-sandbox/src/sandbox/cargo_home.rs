//! The user's cargo settings inside the sandbox, without the user's cargo
//! credentials (decision 0084).
//!
//! The per-user cargo config (`$CARGO_HOME/config.toml`, by default
//! `~/.cargo/config.toml`) can hold a registry token, a credential provider
//! and a proxy URL with a password, so the sandbox never lets a command read
//! it (macOS denies it by name; Linux never grants it). The same file also
//! holds settings a build needs: a source replacement (a mirror or a vendored
//! directory), a linker, rustflags. The dependency fetch runs on the host with
//! the real file, so without those settings an offline build in the sandbox
//! looks for crates under a source it never fetched.
//!
//! So the host reads the file and writes the non-secret keys it names into a
//! cargo home of the command's own, in its temporary directory, and points
//! `CARGO_HOME` there. The registry cache (`registry`, `git`) and installed
//! subcommands (`bin`) are links back to the real cargo home, so cached
//! crates and `cargo <plugin>` still resolve. Cargo reads `$CARGO_HOME`'s
//! config last, as it would read the real one, so a project's own
//! `.cargo/config.toml` still wins over it key by key.
//!
//! Rejected: `CARGO_*` environment variables. Environment values outrank
//! every config file (a project's own linker would lose to the user's), and
//! cargo has no stable environment form for `[source]` tables. A config file
//! in a directory above the worktree: it would outrank the checkout's own
//! config for the same reason.
//!
//! Only these keys are passed: `source.<name>.{replace-with, registry, git,
//! branch, tag, rev, directory, local-registry}`, `registries.<name>.{index,
//! protocol}`, `build.{rustflags, rustdocflags, target-dir}`,
//! `target.<triple or cfg>.{linker, rustflags, rustdocflags}` and
//! `net.offline`. Tokens, credential providers, `[http]`, `[env]` and every
//! other key stay on the host. A source or registry URL that carries a login
//! (`https://user:secret@host`) is withheld too, with every `replace-with`
//! that names it, and the withheld keys are reported with the command
//! ([`super::SandboxedCommand::refused_reads`]).

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_primitives::ambient_authority;
use cap_primitives::fs::{
    DirOptions, FollowSymlinks, OpenOptions, create_dir, open, open_ambient_dir, open_dir_nofollow,
    remove_file, rename, stat,
};
use toml::{Table, Value};

use crate::error::SandboxError;

/// The cargo home made for sandboxed commands, inside their temporary
/// directory.
pub(super) const DIR_NAME: &str = ".kgraf-cargo-home";

/// Entries of the real cargo home linked into the sandbox's one: the
/// registry and git caches an offline build resolves from, and installed
/// subcommands (`cargo nextest`), which cargo finds in `$CARGO_HOME/bin`.
const LINKED: &[&str] = &["registry", "git", "bin"];

/// What [`prepare`] made: the cargo home to point `CARGO_HOME` at, and a
/// line per setting it withheld or could not pass.
#[derive(Debug, Default)]
pub(super) struct Prepared {
    pub home: Option<PathBuf>,
    pub notes: Vec<String>,
}

/// The user's cargo home: `$CARGO_HOME` when set, else `<home>/.cargo`.
pub(super) fn user_cargo_home(home: Option<&Path>) -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".cargo")))
}

/// The config file cargo reads in `cargo_home`: `config` when it exists
/// (cargo prefers it over `config.toml`), else `config.toml`, else none.
fn config_file(cargo_home: &Path) -> Option<PathBuf> {
    ["config", "config.toml"]
        .into_iter()
        .map(|name| cargo_home.join(name))
        .find(|p| p.is_file())
}

/// Build the sandbox's cargo home in `tmp` from the config in `cargo_home`.
///
/// No config file there: nothing is made and `CARGO_HOME` is left alone,
/// since nothing would be lost. A config that cannot be read or parsed:
/// nothing is made, and a note says so (cargo itself fails on it outside the sandbox
/// too, but a command that does not run cargo must not fail for it).
///
/// `tmp` is sandbox-writable, so a command could have left anything in it.
/// Every step works relative to a descriptor opened without following links
/// (the same technique as [`crate::confined`]), and the generated config and
/// links are replaced on every call by an atomic rename, so what a previous
/// command planted there is overwritten, not trusted, and nothing is written
/// outside `tmp`. `tmp` itself is walked no-follow from `root`, the writable
/// root it lies under.
pub(super) fn prepare(
    cargo_home: &Path,
    root: &Path,
    tmp: &Path,
) -> Result<Prepared, SandboxError> {
    let Some(file) = config_file(cargo_home) else {
        return Ok(Prepared::default());
    };
    let skipped = |why: String| {
        Ok(Prepared {
            home: None,
            notes: vec![format!(
                "{} {why}; none of its settings apply inside the sandbox",
                file.display()
            )],
        })
    };
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) => return skipped(format!("cannot be read ({e})")),
    };
    let table = match text.parse::<Table>() {
        Ok(t) => t,
        Err(e) => return skipped(format!("is not valid TOML ({})", e.message().trim())),
    };
    // Cargo resolves a relative path in a config file against the directory
    // that holds the file's directory: the parent of the cargo home.
    let base = cargo_home.parent().unwrap_or(cargo_home);
    let sanitized = sanitize(&table, base);
    let body = toml::to_string(&sanitized.table).map_err(|e| {
        SandboxError::InvalidPolicy(format!("cannot write the sandbox cargo config: {e}"))
    })?;
    let text = format!(
        "# Written by kGraf for sandboxed commands: the non-secret settings of\n\
         # {}. Rewritten before every command; edits here do not last.\n{body}",
        file.display()
    );
    let dir = open_own_dir(root, tmp).map_err(|source| SandboxError::Io {
        path: tmp.join(DIR_NAME),
        source,
    })?;
    let home = tmp.join(DIR_NAME);
    let io_err = |name: &str| {
        let path = home.join(name);
        move |source| SandboxError::Io { path, source }
    };
    // Cargo reads `config` in preference to `config.toml`; one planted
    // there by an earlier command would replace this file.
    remove_entry(&dir, "config").map_err(io_err("config"))?;
    replace_file(&dir, "config.toml", text.as_bytes()).map_err(io_err("config.toml"))?;
    for name in LINKED {
        let target = cargo_home.join(name);
        if target.exists() {
            // A directory here is the cache cargo made on an earlier run,
            // while the user's cargo home had none (it gains one on the first
            // host-side fetch). It is set aside so the link can take its name.
            set_aside_dir(&dir, name).map_err(io_err(name))?;
            replace_link(&dir, name, &target).map_err(io_err(name))?;
        } else {
            // A link to nothing would make cargo fail creating the cache;
            // without one it creates it here, empty, and a directory it made
            // on an earlier run is kept for the next.
            remove_link_or_file(&dir, name).map_err(io_err(name))?;
        }
    }
    let mut notes: Vec<String> = sanitized
        .withheld
        .into_iter()
        .map(|key| {
            format!(
                "{key} in {} is not passed into the sandbox: its URL carries a login",
                file.display()
            )
        })
        .collect();
    notes.dedup();
    Ok(Prepared {
        home: Some(home),
        notes,
    })
}

/// The kept settings and the keys withheld for carrying a login.
#[derive(Debug)]
pub(super) struct Sanitized {
    pub table: Table,
    pub withheld: Vec<String>,
}

/// Keep only the allowlisted keys of a cargo config (module doc), with
/// relative paths made absolute against `base`.
pub(super) fn sanitize(config: &Table, base: &Path) -> Sanitized {
    let mut out = Table::new();
    let mut withheld = Vec::new();
    // Names of sources and registries withheld for a login in their URL:
    // a `replace-with` naming one is withheld too, or cargo would stop on an
    // unknown source name instead of saying why.
    let mut blocked: Vec<String> = Vec::new();

    let mut registries = Table::new();
    for (name, entry) in tables(config.get("registries")) {
        let mut kept = Table::new();
        for (key, value) in entry {
            match key.as_str() {
                "index" if url_has_login(value) => {
                    withheld.push(format!("registries.{name}.index"));
                    blocked.push(name.clone());
                }
                "index" | "protocol" => {
                    kept.insert(key.clone(), value.clone());
                }
                _ => {}
            }
        }
        if !kept.is_empty() {
            registries.insert(name.clone(), Value::Table(kept));
        }
    }

    let mut sources = Table::new();
    for (name, entry) in tables(config.get("source")) {
        let mut kept = Table::new();
        for (key, value) in entry {
            match key.as_str() {
                "registry" | "git" if url_has_login(value) => {
                    withheld.push(format!("source.{name}.{key}"));
                    blocked.push(name.clone());
                }
                "registry" | "git" | "branch" | "tag" | "rev" | "replace-with" => {
                    kept.insert(key.clone(), value.clone());
                }
                "directory" | "local-registry" => {
                    kept.insert(key.clone(), relative_to(value, base, false));
                }
                _ => {}
            }
        }
        sources.insert(name.clone(), Value::Table(kept));
    }
    for name in &blocked {
        sources.remove(name);
        registries.remove(name);
    }
    for (name, entry) in sources.iter_mut() {
        if let Value::Table(t) = entry {
            let names_blocked = t
                .get("replace-with")
                .and_then(Value::as_str)
                .is_some_and(|w| blocked.iter().any(|b| b == w));
            if names_blocked {
                t.remove("replace-with");
                withheld.push(format!("source.{name}.replace-with"));
            }
        }
    }
    sources.retain(|_, v| v.as_table().is_some_and(|t| !t.is_empty()));
    if !sources.is_empty() {
        out.insert("source".into(), Value::Table(sources));
    }
    if !registries.is_empty() {
        out.insert("registries".into(), Value::Table(registries));
    }

    if let Some(Value::Table(build)) = config.get("build") {
        let mut kept = Table::new();
        for (key, value) in build {
            match key.as_str() {
                "rustflags" | "rustdocflags" => {
                    kept.insert(key.clone(), value.clone());
                }
                "target-dir" => {
                    kept.insert(key.clone(), relative_to(value, base, false));
                }
                _ => {}
            }
        }
        if !kept.is_empty() {
            out.insert("build".into(), Value::Table(kept));
        }
    }

    let mut targets = Table::new();
    for (name, entry) in tables(config.get("target")) {
        let mut kept = Table::new();
        for (key, value) in entry {
            match key.as_str() {
                "rustflags" | "rustdocflags" => {
                    kept.insert(key.clone(), value.clone());
                }
                "linker" => {
                    kept.insert(key.clone(), relative_to(value, base, true));
                }
                _ => {}
            }
        }
        if !kept.is_empty() {
            targets.insert(name.clone(), Value::Table(kept));
        }
    }
    if !targets.is_empty() {
        out.insert("target".into(), Value::Table(targets));
    }

    if let Some(offline) = config
        .get("net")
        .and_then(Value::as_table)
        .and_then(|n| n.get("offline"))
    {
        let mut net = Table::new();
        net.insert("offline".into(), offline.clone());
        out.insert("net".into(), Value::Table(net));
    }

    // The registry cache is the user's, linked in and read-only to the
    // sandbox; this cargo home's own last-use record knows nothing of it, so
    // cargo must not try to clean it from here.
    let mut cache = Table::new();
    cache.insert("auto-clean-frequency".into(), Value::String("never".into()));
    out.insert("cache".into(), Value::Table(cache));

    Sanitized {
        table: out,
        withheld,
    }
}

/// The sub-tables of `value`, a table of tables (`[source.<name>]`).
fn tables(value: Option<&Value>) -> impl Iterator<Item = (&String, &Table)> {
    value
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|t| t.iter())
        .filter_map(|(k, v)| v.as_table().map(|t| (k, t)))
}

/// A path value made absolute against `base`, as cargo resolves it where it
/// was written. With `program`, a bare name (no separator) is a program
/// looked up on `PATH` and is kept as written, as cargo does for `linker`.
fn relative_to(value: &Value, base: &Path, program: bool) -> Value {
    match value.as_str() {
        Some(s) if program && !s.contains('/') => value.clone(),
        Some(s) if Path::new(s).is_relative() => {
            Value::String(base.join(s).to_string_lossy().into_owned())
        }
        _ => value.clone(),
    }
}

/// Whether a URL value carries a login: any user part in an `http(s)` URL
/// (`https://TOKEN@host` is a common token form), or a password in any URL.
/// A user name alone in an `ssh://` or scp-like address (`git@host:org/repo`)
/// is not a secret and is kept.
pub(super) fn url_has_login(value: &Value) -> bool {
    let Some(url) = value.as_str() else {
        return false;
    };
    let url = ["sparse+", "registry+", "git+"]
        .iter()
        .find_map(|p| url.strip_prefix(p))
        .unwrap_or(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    scheme == "http" || scheme == "https" || userinfo.contains(':')
}

/// Open `tmp/<DIR_NAME>` for writing, creating it, reached from `root` one
/// no-follow step at a time. An entry at `DIR_NAME` that is not a real
/// directory (a link, a file) is removed first.
fn open_own_dir(root: &Path, tmp: &Path) -> io::Result<File> {
    let rel = tmp.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not under {}", tmp.display(), root.display()),
        )
    })?;
    let mut dir = open_ambient_dir(root, ambient_authority())?;
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a plain relative path", rel.display()),
            ));
        };
        dir = open_dir_nofollow(&dir, Path::new(name))?;
    }
    let name = Path::new(DIR_NAME);
    match stat(&dir, name, FollowSymlinks::No) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            remove_file(&dir, name)?;
            create_dir(&dir, name, &DirOptions::new())?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match create_dir(&dir, name, &DirOptions::new()) {
                Ok(()) => {}
                // Another command's preparation made it first.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }
    open_dir_nofollow(&dir, name)
}

/// A name for a file about to be renamed into place: unique across threads
/// and processes, so concurrent preparations never share one.
fn temp_name(final_name: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".{final_name}.{}.{n}.tmp", std::process::id())
}

/// Write `name` in `dir` as a new file and rename it over what was there.
/// rename(2) replaces the entry itself, never a link's target.
fn replace_file(dir: &File, name: &str, contents: &[u8]) -> io::Result<()> {
    let tmp = temp_name(name);
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    let written = open(dir, Path::new(&tmp), &options).and_then(|mut f| f.write_all(contents));
    let placed = written.and_then(|()| rename(dir, Path::new(&tmp), dir, Path::new(name)));
    if placed.is_err() {
        let _ = remove_file(dir, Path::new(&tmp));
    }
    placed
}

/// Make `name` in `dir` a symbolic link to `target` (absolute), replacing
/// whatever file or link was there. A directory there makes the rename fail:
/// callers set it aside first ([`set_aside_dir`]).
fn replace_link(dir: &File, name: &str, target: &Path) -> io::Result<()> {
    let tmp = temp_name(name);
    let c = |p: &[u8]| {
        CString::new(p).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has a NUL"))
    };
    let target_c = c(target.as_os_str().as_bytes())?;
    let tmp_c = c(tmp.as_bytes())?;
    // SAFETY: both strings are NUL-terminated and live across the call; the
    // descriptor is open for the duration of the borrow of `dir`.
    let rc = unsafe { libc::symlinkat(target_c.as_ptr(), dir.as_raw_fd(), tmp_c.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let placed = rename(dir, Path::new(&tmp), dir, Path::new(name));
    if placed.is_err() {
        let _ = remove_file(dir, Path::new(&tmp));
    }
    placed
}

/// Rename a directory `name` in `dir` to a unique hidden name beside it, so
/// the name is free; anything else there, or nothing, is left alone.
///
/// Renamed, never deleted: the directory was written by sandboxed commands,
/// and a recursive delete from the host would walk what they planted. The
/// renamed copy stays in the command's temporary directory and goes with it
/// when the worktree is removed.
fn set_aside_dir(dir: &File, name: &str) -> io::Result<()> {
    match stat(dir, Path::new(name), FollowSymlinks::No) {
        Ok(m) if m.is_dir() => {
            let aside = temp_name(&format!("{name}.set-aside"));
            rename(dir, Path::new(name), dir, Path::new(&aside))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove a file or link `name` in `dir`; nothing there, or a directory
/// (cargo's own cache, made where no link was), is left as it is.
fn remove_link_or_file(dir: &File, name: &str) -> io::Result<()> {
    match stat(dir, Path::new(name), FollowSymlinks::No) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => remove_file(dir, Path::new(name)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove a file or link `name` in `dir`; nothing there is fine. A directory
/// is left and reported.
fn remove_entry(dir: &File, name: &str) -> io::Result<()> {
    match stat(dir, Path::new(name), FollowSymlinks::No) {
        Ok(m) if m.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is a directory; a sandboxed command made it, remove it"),
        )),
        Ok(_) => remove_file(dir, Path::new(name)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Table {
        text.parse().expect("valid TOML")
    }

    #[test]
    fn only_non_secret_keys_survive() {
        let config = parse(
            r#"
            [registry]
            token = "cio-secret"
            global-credential-providers = ["cargo:token"]
            [registries.corp]
            index = "sparse+https://crates.corp.example/index/"
            token = "corp-secret"
            credential-provider = "cargo:token"
            [source.crates-io]
            replace-with = "vendored"
            [source.vendored]
            directory = "vendor"
            [build]
            rustflags = ["-C", "target-cpu=native"]
            target-dir = "/abs/target"
            jobs = 3
            [target.x86_64-unknown-linux-gnu]
            linker = "clang"
            runner = "evil"
            [target.aarch64-apple-darwin]
            linker = "bin/ld-wrapper"
            [net]
            offline = true
            git-fetch-with-cli = true
            [http]
            proxy = "http://user:pw@proxy:8080"
            [env]
            API_KEY = "env-secret"
            [alias]
            b = "build"
            "#,
        );
        let s = sanitize(&config, Path::new("/home/u"));
        let text = toml::to_string(&s.table).expect("serializes");
        for secret in [
            "cio-secret",
            "corp-secret",
            "env-secret",
            "pw@proxy",
            "cargo:token",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        for gone in [
            "[registry]",
            "[http]",
            "[env]",
            "[alias]",
            "runner",
            "jobs",
            "git-fetch",
        ] {
            assert!(!text.contains(gone), "{gone} kept:\n{text}");
        }
        let t = &s.table;
        assert_eq!(
            t["source"]["crates-io"]["replace-with"].as_str(),
            Some("vendored")
        );
        assert_eq!(
            t["source"]["vendored"]["directory"].as_str(),
            Some("/home/u/vendor"),
            "a relative path is made absolute where cargo would resolve it"
        );
        assert_eq!(
            t["registries"]["corp"]["index"].as_str(),
            Some("sparse+https://crates.corp.example/index/")
        );
        assert_eq!(t["build"]["rustflags"].as_array().map(Vec::len), Some(2));
        assert_eq!(t["build"]["target-dir"].as_str(), Some("/abs/target"));
        assert_eq!(
            t["target"]["x86_64-unknown-linux-gnu"]["linker"].as_str(),
            Some("clang"),
            "a bare program name stays a PATH lookup"
        );
        assert_eq!(
            t["target"]["aarch64-apple-darwin"]["linker"].as_str(),
            Some("/home/u/bin/ld-wrapper")
        );
        assert_eq!(t["net"]["offline"].as_bool(), Some(true));
        assert_eq!(t["cache"]["auto-clean-frequency"].as_str(), Some("never"));
        assert!(s.withheld.is_empty(), "{:?}", s.withheld);
    }

    #[test]
    fn a_url_with_a_login_is_withheld_with_what_names_it() {
        let config = parse(
            r#"
            [source.crates-io]
            replace-with = "mirror"
            [source.mirror]
            registry = "sparse+https://ci:tok3n@mirror.example/index/"
            [registries.priv]
            index = "https://TOKEN123@git.example/index"
            [source.other]
            replace-with = "priv"
            "#,
        );
        let s = sanitize(&config, Path::new("/home/u"));
        let text = toml::to_string(&s.table).expect("serializes");
        assert!(
            !text.contains("tok3n") && !text.contains("TOKEN123"),
            "{text}"
        );
        assert!(!text.contains("replace-with"), "{text}");
        let mut withheld = s.withheld.clone();
        withheld.sort();
        assert_eq!(
            withheld,
            [
                "registries.priv.index",
                "source.crates-io.replace-with",
                "source.mirror.registry",
                "source.other.replace-with",
            ]
        );
    }

    #[test]
    fn login_detection() {
        let v = |s: &str| Value::String(s.into());
        for with in [
            "https://user:pw@host/x",
            "https://TOKEN@host/x",
            "sparse+https://u@host/index/",
            "ssh://git:pw@host/repo",
        ] {
            assert!(url_has_login(&v(with)), "{with}");
        }
        for without in [
            "https://host/x",
            "sparse+https://host/index/",
            "ssh://git@host/repo",
            "git@github.com:org/repo.git",
            "https://host/path@v1",
        ] {
            assert!(!url_has_login(&v(without)), "{without}");
        }
    }

    /// A temp directory standing in for the user's home, with a cargo home
    /// holding `config` text and a registry cache.
    fn user_home(config: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().expect("tempdir");
        let cargo = home.path().join(".cargo");
        std::fs::create_dir_all(cargo.join("registry")).expect("registry");
        std::fs::write(cargo.join("config.toml"), config).expect("config");
        std::fs::write(
            cargo.join("credentials.toml"),
            "[registry]\ntoken = \"cred-secret\"\n",
        )
        .expect("credentials");
        (home, cargo)
    }

    #[test]
    fn prepare_writes_the_config_and_links_the_cache() {
        let (_home, cargo) = user_home(
            "[registry]\ntoken = \"cio-secret\"\n[source.crates-io]\nreplace-with = \"v\"\n\
             [source.v]\ndirectory = \"/srv/vendor\"\n",
        );
        let root = tempfile::tempdir().expect("root");
        let root = kgraf_core::plain_path::canonicalize(root.path()).expect("canonical");
        let tmp = root.join("scratch");
        std::fs::create_dir(&tmp).expect("scratch");
        let p = prepare(&cargo, &root, &tmp).expect("prepared");
        let made = p.home.expect("a cargo home");
        assert_eq!(made, tmp.join(DIR_NAME));
        let written = std::fs::read_to_string(made.join("config.toml")).expect("config");
        assert!(written.contains("/srv/vendor"), "{written}");
        assert!(!written.contains("cio-secret"), "{written}");
        assert!(!made.join("credentials.toml").exists());
        assert_eq!(
            std::fs::read_link(made.join("registry")).expect("link"),
            cargo.join("registry")
        );
        assert!(
            std::fs::symlink_metadata(made.join("git")).is_err(),
            "no link to a cache that does not exist"
        );
        assert!(p.notes.is_empty(), "{:?}", p.notes);
    }

    /// What a sandboxed command leaves in its cargo home is replaced, and a
    /// link it planted is replaced as a link: nothing is written through it.
    #[test]
    fn planted_entries_are_replaced_not_followed() {
        let (_home, cargo) = user_home("[net]\noffline = true\n");
        let root = tempfile::tempdir().expect("root");
        let root = kgraf_core::plain_path::canonicalize(root.path()).expect("canonical");
        let tmp = root.join("scratch");
        let own = tmp.join(DIR_NAME);
        std::fs::create_dir_all(&own).expect("own");
        let outside = tempfile::tempdir().expect("outside");
        let victim = outside.path().join("victim.toml");
        std::fs::write(&victim, "keep").expect("victim");
        std::os::unix::fs::symlink(&victim, own.join("config.toml")).expect("planted link");
        std::fs::write(own.join("config"), "[target.x]\nrunner = \"evil\"\n").expect("planted");
        std::os::unix::fs::symlink(outside.path(), own.join("registry")).expect("planted");

        prepare(&cargo, &root, &tmp).expect("prepared");

        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "keep");
        assert!(!own.join("config").exists(), "planted config removed");
        let meta = std::fs::symlink_metadata(own.join("config.toml")).expect("config");
        assert!(meta.is_file(), "a regular file, not the planted link");
        assert_eq!(
            std::fs::read_link(own.join("registry")).expect("link"),
            cargo.join("registry")
        );
    }

    /// A cache cargo made in the sandbox's cargo home while the user's had
    /// none is kept for the next command, and once the user's cargo home has
    /// one (the first host-side fetch made it), it is set aside, not deleted,
    /// and the link takes its name. Before, the second command after cargo
    /// made one failed on every run.
    #[test]
    fn a_cache_cargo_made_here_is_kept_then_set_aside_for_the_link() {
        let (_home, cargo) = user_home("[net]\noffline = true\n");
        std::fs::remove_dir(cargo.join("registry")).expect("no user cache yet");
        let root = tempfile::tempdir().expect("root");
        let root = kgraf_core::plain_path::canonicalize(root.path()).expect("canonical");
        let tmp = root.join("scratch");
        std::fs::create_dir(&tmp).expect("scratch");
        let own = tmp.join(DIR_NAME);

        prepare(&cargo, &root, &tmp).expect("first command");
        // What cargo does in the sandbox when there is no link: it creates
        // its own cache.
        std::fs::create_dir_all(own.join("registry/index")).expect("cargo's cache");
        std::fs::write(own.join("registry/index/entry"), "x").expect("entry");

        prepare(&cargo, &root, &tmp).expect("a later command, no user cache");
        assert!(
            own.join("registry/index/entry").is_file(),
            "cargo's own cache is kept while there is nothing to link"
        );

        std::fs::create_dir(cargo.join("registry")).expect("the host fetched");
        prepare(&cargo, &root, &tmp).expect("a later command, user cache");
        assert_eq!(
            std::fs::read_link(own.join("registry")).expect("link"),
            cargo.join("registry")
        );
        let aside: Vec<_> = std::fs::read_dir(&own)
            .expect("list")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.join("index/entry").is_file())
            .collect();
        assert_eq!(aside.len(), 1, "set aside, not deleted: {aside:?}");
        assert!(
            std::fs::read_dir(cargo.join("registry"))
                .expect("list")
                .next()
                .is_none(),
            "nothing moved into the user's cache"
        );
    }

    /// The cargo home directory itself swapped for a link to a directory
    /// outside: the link is removed, never written through.
    #[test]
    fn a_linked_cargo_home_is_not_followed() {
        let (_home, cargo) = user_home("[net]\noffline = true\n");
        let root = tempfile::tempdir().expect("root");
        let root = kgraf_core::plain_path::canonicalize(root.path()).expect("canonical");
        let tmp = root.join("scratch");
        std::fs::create_dir(&tmp).expect("scratch");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), tmp.join(DIR_NAME)).expect("planted");

        prepare(&cargo, &root, &tmp).expect("prepared");

        assert_eq!(
            std::fs::read_dir(outside.path()).expect("list").count(),
            0,
            "nothing written through the link"
        );
        assert!(tmp.join(DIR_NAME).join("config.toml").is_file());
    }

    #[test]
    fn no_config_makes_nothing_and_a_bad_one_is_reported() {
        let home = tempfile::tempdir().expect("home");
        let cargo = home.path().join(".cargo");
        std::fs::create_dir_all(&cargo).expect("cargo");
        let root = tempfile::tempdir().expect("root");
        let root = kgraf_core::plain_path::canonicalize(root.path()).expect("canonical");
        let p = prepare(&cargo, &root, &root).expect("prepared");
        assert!(p.home.is_none() && p.notes.is_empty());
        assert!(!root.join(DIR_NAME).exists());

        std::fs::write(cargo.join("config.toml"), "[source\n").expect("bad");
        let p = prepare(&cargo, &root, &root).expect("prepared");
        assert!(p.home.is_none());
        assert!(p.notes[0].contains("not valid TOML"), "{:?}", p.notes);
    }

    #[test]
    fn cargo_prefers_config_over_config_toml() {
        let (_home, cargo) = user_home("[net]\noffline = true\n");
        std::fs::write(cargo.join("config"), "[net]\noffline = false\n").expect("config");
        assert_eq!(config_file(&cargo), Some(cargo.join("config")));
    }
}
