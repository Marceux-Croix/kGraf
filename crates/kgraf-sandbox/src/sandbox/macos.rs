//! macOS backend: Seatbelt through `/usr/bin/sandbox-exec`.
//!
//! Policy assembly is a subset port of openai/codex
//! `codex-rs/sandboxing/src/seatbelt.rs`, commit
//! e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0, Copyright 2025
//! OpenAI (see `third_party/codex/UPSTREAM`). The three `.sbpl` files come
//! from the same commit; the network one is modified (no SecurityServer).
//!
//! Departures from codex's full-network mode, each closing a hole found by a
//! live probe:
//! - `(allow network-outbound)` also allowed `connect()` to any unix socket,
//!   including `$SSH_AUTH_SOCK`. Outbound is now IP only; unix sockets are
//!   allowed only under the writable roots and an explicit allowlist.
//! - `com.apple.SecurityServer` let `git credential-osxkeychain` read the
//!   user's keychain. Removed.
//! - Reads were fully open. Credential stores, browser profiles, messages and
//!   shell histories under `$HOME` are now denied.
//!
//! No permissive Rust wrapper for Seatbelt exists (`birdcage` is GPL and
//! archived), so the launcher is called directly.
//! `sandbox-exec` is marked deprecated by Apple but is what codex, Claude Code
//! and Chromium's tooling use; its removal would surface as
//! [`SandboxError::LauncherMissing`], never as an unconfined run.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::child::ChildStep;
use super::{Backend, IsolationStrength, Network, ReadConfinement, Resolved};
use crate::error::SandboxError;

pub(super) const STRENGTH: IsolationStrength = IsolationStrength {
    backend: Backend::MacosSeatbelt,
    writes_confined: true,
    reads: ReadConfinement::CredentialDenyList,
    unix_sockets_confined: true,
    network_deniable: true,
    gitlink_protected: true,
    git_config_freezable: true,
    gaps: "reads outside the credential deny list are allowed (source code in other \
           repositories, most of $HOME), apart from the checkout a worktree was made \
           from, whose files other than its cargo files and its installed dependency trees \
           (read, never written) can be stat()ed but not read; \
           a process that closes its inherited descriptors and starts a new session \
           outlives its command unseen; allowed mach services are reachable; \
           the state directories on the path to the worktree can be stat()ed \
           (mode, owner, mtime, link count and size show how many worktrees \
           exist and when that changed, not their names or contents); a \
           guessed name under the state directory is confirmed or refuted by \
           its error (operation not permitted if it exists, no such file if \
           not); no resource limits",
};

/// Fixed path, never `PATH` lookup, so a planted `sandbox-exec` earlier on
/// `PATH` cannot replace the launcher (codex does the same).
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");
const NETWORK_POLICY: &str = include_str!("seatbelt_network_policy.sbpl");
const PREFERENCES_POLICY: &str = include_str!("seatbelt_preferences_policy.sbpl");

/// The system DNS resolver socket. Needed for name resolution with network on.
const MDNS_SOCKET: &str = "/private/var/run/mDNSResponder";

/// Files in a cargo home outside `$HOME` whose reads are denied: the
/// credentials and the config, which can carry a token or a proxy login.
const CARGO_HOME_SECRET_FILES: &[&str] =
    &["credentials", "credentials.toml", "config", "config.toml"];

/// Paths under `$HOME` whose reads are denied: keys, cloud and registry
/// credentials, keychains, browser profiles, mail and messages, histories.
pub(super) const DENIED_HOME_READS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    // Git's credential-store file under the XDG path. The rest of
    // `~/.config/git` stays readable: git aborts on a config file it may not
    // read, and builds run `git` for version stamps.
    ".config/git/credentials",
    // HashiCorp Vault's token helper file, PostgreSQL passwords, Terraform
    // CLI credentials, RubyGems credentials, OpenAI Codex CLI auth.
    ".vault-token",
    ".pgpass",
    ".terraform.d",
    ".gem/credentials",
    ".codex",
    ".password-store",
    ".pypirc",
    ".config/gh",
    ".config/gcloud",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    // kGraf's own per-user data: the diagnostics key, the cross-process rate
    // limiter, OAuth locks. A path spelled in another case reaches the same
    // directory on a case-insensitive volume and is denied too (measured).
    "Library/Application Support/kGraf",
    "Library/Caches/kGraf",
    // Another coding agent's own directory: its stored login, session
    // backups and histories. Its configuration file beside it, and every
    // sibling path that starts with its name (a `.backup` or `.tmp.NNNN`
    // copy an editor or the agent itself leaves next to it), is denied below
    // by a regex: a plain `subpath` entry only matches that one literal path
    // and its own descendants, not a sibling file whose name happens to
    // start the same way.
    ".claude",
    // Maven and Gradle repository passwords, Podman registry logins, Hugging
    // Face tokens (old and current location), the 1Password CLI, Oracle
    // Cloud keys, Bundler's per-user config with private gem source logins.
    // The Maven and Gradle caches beside them stay readable for builds.
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".gradle/gradle.properties",
    // Gradle init scripts (they can carry repository passwords) and NuGet's
    // per-user configuration (package source passwords), at its macOS and
    // its XDG location. The package stores beside them stay readable:
    // `~/.gradle/caches`, `~/.gradle/wrapper/dists`, `~/.nuget/packages`.
    ".gradle/init.d",
    ".gradle/init.gradle",
    ".gradle/init.gradle.kts",
    ".nuget/NuGet/NuGet.Config",
    ".config/NuGet/NuGet.Config",
    ".config/containers/auth.json",
    ".huggingface",
    ".cache/huggingface/token",
    ".config/op",
    ".op",
    ".oci",
    ".bundle/config",
    // Registry tokens. The sandbox points NPM_CONFIG_USERCONFIG at an empty
    // file in the scratch directory, so npm never looks here.
    ".npmrc",
    // Package index logins: pip's config (an index URL can carry a password),
    // Yarn's registry tokens, Poetry's repository passwords.
    ".pip/pip.conf",
    ".config/pip/pip.conf",
    ".yarnrc",
    ".yarnrc.yml",
    ".config/pypoetry/auth.toml",
    // The same tools' macOS locations (pip's and Poetry's per-user folders
    // under Application Support), pnpm's global config (`pnpm login` writes
    // its tokens there), Bun's global config (registry tokens), uv's
    // credential store (`uv auth login`), and Gradle's Develocity access
    // keys (old and current folder name). Each is a login a build reads
    // from the user's home, so a test that can read it can send it on.
    "Library/Application Support/pip/pip.conf",
    "Library/Application Support/pypoetry/auth.toml",
    "Library/Preferences/pnpm/rc",
    ".bunfig.toml",
    ".local/share/uv/credentials",
    ".gradle/enterprise",
    ".gradle/develocity",
    // Cargo's per-user config can hold a registry token and a proxy URL with
    // a password. Seatbelt denies the stat too, and cargo skips a config file
    // it cannot stat (measured). Its non-secret settings (a source
    // replacement, a linker, rustflags) reach the command through a cargo
    // home of its own that the host writes (`super::cargo_home`), as on
    // Linux, where the file is never granted.
    ".cargo/config.toml",
    ".cargo/config",
    // Docker's legacy login file, s3cmd and boto (AWS keys), Databricks
    // tokens, rclone remotes, and the DigitalOcean, Heroku, GitLab, Helm
    // repository and Stripe CLI logins.
    ".dockercfg",
    ".s3cfg",
    ".boto",
    ".databrickscfg",
    ".config/rclone",
    ".config/doctl",
    ".config/heroku",
    ".config/glab-cli",
    ".config/helm",
    ".config/stripe",
    "Library/Keychains",
    "Library/Cookies",
    "Library/Messages",
    "Library/Mail",
    "Library/Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Arc",
    ".zsh_history",
    ".zsh_sessions",
    ".bash_history",
    ".bash_sessions",
    ".python_history",
    ".node_repl_history",
    ".psql_history",
    ".mysql_history",
    ".sqlite_history",
    ".lesshst",
    ".local/share/fish/fish_history",
];

/// Escape a literal string for use inside a Seatbelt `regex` filter (an
/// NSRegularExpression pattern). `$HOME` is attacker-influenced on a shared
/// machine only in the sense that its value comes from the environment, not
/// from this process, so any regex metacharacter it may contain (most
/// plausibly `+` or `.` in an unusual account name) is escaped rather than
/// trusted.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(super) fn command(
    policy: &Resolved,
    program: &OsStr,
) -> Result<(Command, Option<ChildStep>), SandboxError> {
    let launcher = Path::new(SANDBOX_EXEC);
    if !launcher.is_file() {
        return Err(SandboxError::LauncherMissing(launcher.to_path_buf()));
    }
    let home = policy.home.as_deref().ok_or_else(|| {
        SandboxError::InvalidPolicy(
            "HOME is unset; cannot build the credential read deny list".into(),
        )
    })?;
    let (profile, params) = profile(policy, home);
    let mut cmd = Command::new(launcher);
    cmd.arg("-p").arg(profile);
    for (key, value) in params {
        let mut define = std::ffi::OsString::from(format!("-D{key}="));
        define.push(value.as_os_str());
        cmd.arg(define);
    }
    cmd.arg("--").arg(program);
    Ok((cmd, None))
}

/// Build the profile text and its `-D` parameters. Paths travel as
/// parameters, not interpolated text, so a path containing `"` or `)` cannot
/// alter the profile.
pub(super) fn profile(policy: &Resolved, home: &Path) -> (String, Vec<(String, PathBuf)>) {
    let mut params = Vec::new();
    let mut write_allows = Vec::new();
    let mut socket_rules = vec!["(allow system-socket (socket-domain AF_UNIX))".to_string()];
    let mut write_denies = Vec::new();
    for (i, root) in policy.writable.iter().enumerate() {
        let key = format!("WRITABLE_ROOT_{i}");
        let gitlink_key = format!("WRITABLE_ROOT_{i}_GIT");
        write_allows.push(format!("(subpath (param \"{key}\"))"));
        // Unix sockets created inside the sandbox's own roots stay usable.
        socket_rules.push(format!(
            "(allow network-bind (local unix-socket (subpath (param \"{key}\"))))"
        ));
        socket_rules.push(format!(
            "(allow network-outbound (remote unix-socket (subpath (param \"{key}\"))))"
        ));
        // A worker must not delete or replace the root itself (codex anchor deny).
        write_denies.push(format!(
            "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))"
        ));
        // Nor rewrite the gitlink, which would aim host-side git elsewhere.
        write_denies.push(format!(
            "(deny file-write* (subpath (param \"{gitlink_key}\")))"
        ));
        params.push((key, root.clone()));
        params.push((gitlink_key, root.join(".git")));
    }
    for (i, socket) in policy.unix_sockets.iter().enumerate() {
        let key = format!("UNIX_SOCKET_{i}");
        socket_rules.push(format!(
            "(allow network-outbound (remote unix-socket (subpath (param \"{key}\"))))"
        ));
        params.push((key, socket.clone()));
    }
    // kGraf's own state (SandboxPolicy::deny_read), and the checkout a
    // worktree was made from (SandboxPolicy::with_ancestor_reads), then the
    // roots the command works in re-allowed on top: a worktree lives under
    // the state directory and the checkout it may not otherwise read. The
    // checkout is denied because macOS reads are otherwise open: a build
    // script could read a `.env` with vendor keys at the checkout root and
    // print it into a tool result that goes to a model provider.
    //
    // The checkout keeps its metadata readable (stat, never contents or
    // listings): cargo, looking for a workspace above a package, parses each
    // ancestor `Cargo.toml` it finds, and that parse checks that the
    // manifest's `src/main.rs` or `src/lib.rs` exists.
    let mut extra_denies = Vec::new();
    let mut denied: Vec<&Path> = policy.read_denied.iter().map(PathBuf::as_path).collect();
    for (i, dir) in policy.read_denied.iter().enumerate() {
        let key = format!("DENY_STATE_READ_{i}");
        extra_denies.push(format!("(subpath (param \"{key}\"))"));
        params.push((key, dir.to_path_buf()));
    }
    // A hidden folder (SandboxPolicy::hide) lies beneath a writable root,
    // whose own allows come first; its denies follow them, and the roots
    // beneath it are allowed again after its denies. The folders between it
    // and such a root keep their metadata readable (realpath(3) needs it),
    // never their listing or contents.
    let mut hidden_writes = Vec::new();
    let mut hidden_reads = Vec::new();
    for (i, hidden) in policy.hidden.iter().enumerate() {
        let key = format!("HIDDEN_{i}");
        hidden_reads.push(format!("(deny file-read* (subpath (param \"{key}\")))"));
        hidden_writes.push(format!("(deny file-write* (subpath (param \"{key}\")))"));
        params.push((key, hidden.dir.clone()));
        let mut between = Vec::new();
        for (j, keep) in hidden.keep.iter().enumerate() {
            let key = format!("HIDDEN_{i}_KEEP_{j}");
            hidden_reads.push(format!("(allow file-read* (subpath (param \"{key}\")))"));
            hidden_writes.push(format!("(allow file-write* (subpath (param \"{key}\")))"));
            params.push((key, keep.clone()));
            for dir in keep.ancestors().skip(1) {
                if !dir.starts_with(&hidden.dir) {
                    break;
                }
                if !between.contains(&dir) {
                    between.push(dir);
                }
            }
        }
        for (j, dir) in between.into_iter().enumerate() {
            let key = format!("HIDDEN_{i}_ABOVE_{j}");
            hidden_reads.push(format!(
                "(allow file-read-metadata (literal (param \"{key}\")))"
            ));
            params.push((key, dir.to_path_buf()));
        }
    }
    let checkout = policy.ancestor_limit.as_deref();
    if let Some(limit) = checkout {
        denied.push(limit);
        params.push(("DENY_CHECKOUT_READ".to_string(), limit.to_path_buf()));
    }
    let mut root_reads = Vec::new();
    for (i, root) in policy.readable.iter().enumerate() {
        let key = format!("READABLE_ROOT_{i}");
        root_reads.push(format!("(subpath (param \"{key}\"))"));
        params.push((key, root.clone()));
    }
    for i in 0..policy.writable.len() {
        root_reads.push(format!("(subpath (param \"WRITABLE_ROOT_{i}\"))"));
    }
    // realpath(3) lstat()s every component of the path it resolves, so a
    // root beneath a denied directory could not be canonicalized: cargo
    // canonicalizes the package root and the target directory before it
    // writes dep-info, and failed with EPERM on every build in a worktree
    // under `.kgraf`. Re-allow the
    // metadata, and only the metadata, of each denied ancestor directory:
    // `literal` matches the directory itself, so its listing
    // (file-read-data) and every other entry beneath it stay denied.
    let mut ancestors: Vec<&Path> = Vec::new();
    for root in policy.writable.iter().chain(&policy.readable) {
        for dir in root.ancestors().skip(1) {
            let is_denied = denied.iter().any(|d| dir.starts_with(d));
            if is_denied && !ancestors.contains(&dir) {
                ancestors.push(dir);
            }
        }
    }
    let mut ancestor_metadata = Vec::new();
    for (i, dir) in ancestors.into_iter().enumerate() {
        let key = format!("DENIED_ANCESTOR_{i}");
        ancestor_metadata.push(format!("(literal (param \"{key}\"))"));
        params.push((key, dir.to_path_buf()));
    }
    // The cargo files cargo reads on its way up from a writable root to the
    // checkout (`super::ANCESTOR_READS`), each as one literal path: never one
    // beneath kGraf's state or in a cargo home, which may hold credentials.
    let mut ancestor_files = Vec::new();
    if let Some(limit) = policy.ancestor_limit.as_deref() {
        let mut files: Vec<PathBuf> = Vec::new();
        for root in &policy.writable {
            for dir in root.ancestors().skip(1) {
                if !dir.starts_with(limit) {
                    break;
                }
                for rel in super::ANCESTOR_READS {
                    let file = dir.join(rel);
                    let under_state = policy.read_denied.iter().any(|d| file.starts_with(d));
                    let in_cargo_home = match &policy.cargo_homes {
                        Some(homes) => homes.iter().any(|h| file.starts_with(h)),
                        None => rel.starts_with(".cargo"),
                    };
                    if !under_state && !in_cargo_home && !files.contains(&file) {
                        files.push(file);
                    }
                }
            }
        }
        for (i, file) in files.into_iter().enumerate() {
            let key = format!("ANCESTOR_FILE_{i}");
            ancestor_files.push(format!("(literal (param \"{key}\"))"));
            params.push((key, file));
        }
    }
    let mut read_denies = Vec::new();
    let mut home_denied: Vec<PathBuf> = Vec::new();
    for (i, rel) in DENIED_HOME_READS.iter().enumerate() {
        let key = format!("DENY_READ_{i}");
        read_denies.push(format!("(subpath (param \"{key}\"))"));
        let path = home.join(rel);
        // Seatbelt matches the resolved path, so a symlinked `~/.ssh` is
        // readable at its target unless the target is denied too. The
        // spelled path stays denied as well, for a path that does not exist
        // yet or a link created after this profile was built.
        if let Ok(real) = kgraf_core::plain_path::canonicalize(&path)
            && real != path
        {
            let real_key = format!("DENY_READ_REAL_{i}");
            read_denies.push(format!("(subpath (param \"{real_key}\"))"));
            home_denied.push(real.clone());
            params.push((real_key, real));
        }
        home_denied.push(path.clone());
        params.push((key, path));
    }
    let (inner_roots, inner_ancestors) = roots_inside_home_denies(policy, &home_denied);
    let mut inner_reads = Vec::new();
    for (i, root) in inner_roots.into_iter().enumerate() {
        let key = format!("HOME_DENIED_ROOT_{i}");
        inner_reads.push(format!("(subpath (param \"{key}\"))"));
        params.push((key, root));
    }
    let mut inner_metadata = Vec::new();
    for (i, dir) in inner_ancestors.into_iter().enumerate() {
        let key = format!("HOME_DENIED_ANCESTOR_{i}");
        inner_metadata.push(format!("(literal (param \"{key}\"))"));
        params.push((key, dir));
    }
    // A cargo home outside the home folder (`$CARGO_HOME`) holds the same
    // registry token and proxy login as `~/.cargo`, which the list above
    // denies by name. Reads are open here, so its files are denied by name.
    for (i, cargo_home) in policy
        .cargo_homes
        .iter()
        .flatten()
        .filter(|c| !c.starts_with(home))
        .enumerate()
    {
        for (j, file) in CARGO_HOME_SECRET_FILES.iter().enumerate() {
            let key = format!("DENY_READ_CARGO_HOME_{i}_{j}");
            read_denies.push(format!("(literal (param \"{key}\"))"));
            params.push((key, cargo_home.join(file)));
        }
    }
    // Another coding agent's configuration file, plus every sibling path
    // that starts with its name: `.claude.json.backup`, `.claude.json.tmp.*`
    // and any other copy an editor or the agent itself leaves next to it.
    // `subpath` cannot express this (it matches only the literal path it is
    // given, and its descendants, never a sibling), so this is a Seatbelt
    // `regex` filter instead, anchored to the start of the path and built at
    // runtime from `home` since a `.sbpl` file is fixed at compile time.
    let claude_json_regex = format!(
        "^{}/\\.claude\\.json",
        regex_escape(&home.display().to_string())
    );
    let claude_json_key = "DENY_READ_CLAUDE_JSON_REGEX".to_string();
    read_denies.push(format!("(regex (param \"{claude_json_key}\"))"));
    params.push((claude_json_key, PathBuf::from(claude_json_regex)));

    let mut sections = vec![BASE_POLICY.to_string(), "(allow file-read*)".to_string()];
    if checkout.is_some() {
        // Before the state deny, so the metadata allow never reopens the
        // state directory's metadata.
        sections.push(
            "; the checkout\n(deny file-read* (subpath (param \"DENY_CHECKOUT_READ\")))\n\
             (allow file-read-metadata (subpath (param \"DENY_CHECKOUT_READ\")))"
                .to_string(),
        );
    }
    if !extra_denies.is_empty() || checkout.is_some() {
        if !extra_denies.is_empty() {
            sections.push(format!(
                "; kGraf state\n(deny file-read*\n  {}\n)",
                extra_denies.join("\n  ")
            ));
        }
        sections.push(format!(
            "(allow file-read*\n  {}\n)",
            root_reads.join("\n  ")
        ));
        if !ancestor_metadata.is_empty() {
            sections.push(format!(
                "(allow file-read-metadata\n  {}\n)",
                ancestor_metadata.join("\n  ")
            ));
        }
        if !ancestor_files.is_empty() {
            sections.push(format!(
                "; cargo files above a root\n(allow file-read*\n  {}\n)",
                ancestor_files.join("\n  ")
            ));
        }
    }
    // The checkout's installed dependency trees, linked into the tree
    // (`SandboxPolicy::with_dependency_links`): read and run, never written
    // (no write rule names them). After the checkout deny, which they lie
    // beneath, and before the credential deny, so a link inside a tree that
    // points at a credential store is still denied at its target.
    if !policy.dependency_reads.is_empty() {
        let mut dependency_reads = Vec::new();
        for (i, tree) in policy.dependency_reads.iter().enumerate() {
            let key = format!("DEPENDENCY_READ_{i}");
            dependency_reads.push(format!("(subpath (param \"{key}\"))"));
            params.push((key, tree.clone()));
        }
        sections.push(format!(
            "; installed dependency trees\n(allow file-read*\n  {}\n)",
            dependency_reads.join("\n  ")
        ));
    }
    // After every allow above (the writable root around a hidden folder is
    // re-allowed there), before the credential deny.
    if !hidden_reads.is_empty() {
        sections.push(format!(
            "; hidden folders, and the roots beneath them\n{}",
            hidden_reads.join("\n")
        ));
    }
    // After the re-allow, so no root reopens a credential store.
    sections.extend([format!(
        "; credential stores, browser profiles, histories\n(deny file-read*\n  {}\n)",
        read_denies.join("\n  ")
    )]);
    // A root that lies strictly inside a denied home path (a project kept in
    // `~/.claude`, whose worktrees and shared git directory sit there too) is
    // re-allowed after the deny, or the command could not read its own tree.
    // Only the root is reopened, plus the metadata (never the listing) of
    // each directory between it and the denied path, which realpath(3)
    // needs. A root that contains a denied path, such as HOME itself, is not
    // in this list, so it never reopens a credential store.
    if !inner_reads.is_empty() {
        sections.push(format!(
            "; roots inside a denied home path\n(allow file-read*\n  {}\n)",
            inner_reads.join("\n  ")
        ));
        sections.push(format!(
            "(allow file-read-metadata\n  {}\n)",
            inner_metadata.join("\n  ")
        ));
    }
    sections.push(format!(
        "(allow file-write*\n  {}\n)",
        write_allows.join("\n  ")
    ));
    // After the root allows, so the hidden folder's deny wins over the root
    // above it, and its own roots are allowed again after the deny; before
    // the anchor and repository-link denies, which still win over both.
    if !hidden_writes.is_empty() {
        sections.push(format!(
            "; hidden folders and the roots beneath them\n{}",
            hidden_writes.join("\n")
        ));
    }
    sections.push(socket_rules.join("\n"));
    match policy.network {
        Network::Allow => {
            // IP only: a bare (allow network-outbound) also admits unix sockets.
            sections.push(
                "(allow network-outbound (remote ip \"*:*\"))\n\
                 (allow network-inbound (local ip \"*:*\"))\n\
                 (allow network-bind (local ip \"*:*\"))"
                    .to_string(),
            );
            sections.push(NETWORK_POLICY.to_string());
            sections.push(format!(
                "(allow network-outbound (remote unix-socket (literal \"{MDNS_SOCKET}\")))"
            ));
        }
        Network::Proxy { port } => {
            // One loopback port, kGraf's egress proxy, and nothing else: no
            // other IP destination, no bind, and no DNS resolver socket (a
            // name lookup is a way out too; the proxy resolves names). The
            // form is the one Anthropic's sandbox-runtime uses for its proxy
            // (`(remote ip "localhost:PORT")`, which matches 127.0.0.1 and
            // ::1). The mach services of the network policy stay: TLS
            // certificate checks of the tunneled connection need trustd.
            sections.push(format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))"
            ));
            sections.push(NETWORK_POLICY.to_string());
        }
        Network::Deny => {}
    }
    sections.push(PREFERENCES_POLICY.to_string());
    sections.push("(deny mach-lookup (xpc-service-name-prefix \"\"))".to_string());
    // Write denies last so no broader allow reopens them.
    sections.extend(write_denies);
    let frozen = frozen_git_regexes(policy);
    if !frozen.is_empty() {
        let mut rules = Vec::new();
        for (i, regex) in frozen.into_iter().enumerate() {
            let key = format!("FROZEN_GIT_{i}");
            rules.push(format!("(regex (param \"{key}\"))"));
            params.push((key, PathBuf::from(regex)));
        }
        sections.push(format!(
            "; git configuration inside a writable git directory (decision 0094)\n\
             (deny file-write*\n  {}\n)",
            rules.join("\n  ")
        ));
    }
    // F_MAKECOMPRESSED (80) and F_TRANSFEREXTENTS (110) mutate files through
    // read-only descriptors, bypassing file-write* (codex).
    sections.push("(deny system-fcntl (fcntl-command 80 110))".to_string());
    (sections.join("\n"), params)
}

/// The anchored regexes for every frozen git file and every directory
/// above it, up to (not including) the root it lies in. Freezing the
/// directories too stops a command from renaming one away and moving in a
/// replacement built elsewhere holding its own config; a write inside such a
/// directory is another path and stays allowed. A listed file outside every
/// writable root is already read-only and gets no rule.
fn frozen_git_regexes(policy: &Resolved) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |root: &Path, segments: &[String]| {
        let base = regex_escape(&root.display().to_string());
        for end in 1..=segments.len() {
            let regex = format!("^{base}/{}$", segments[..end].join("/"));
            if !out.contains(&regex) {
                out.push(regex);
            }
        }
    };
    for frozen in &policy.frozen_git {
        for entry in super::FROZEN_GIT_FILES {
            let segments: Vec<String> = entry
                .split('/')
                .map(|seg| {
                    if seg == "*" {
                        "[^/]+".to_string()
                    } else {
                        regex_escape(seg)
                    }
                })
                .collect();
            push(&frozen.common_dir, &segments);
        }
        for file in &frozen.files {
            let Some(root) = policy
                .writable
                .iter()
                .filter(|r| file.starts_with(r) && file != *r)
                .max_by_key(|r| r.components().count())
            else {
                continue;
            };
            let Ok(rel) = file.strip_prefix(root) else {
                continue;
            };
            let segments: Vec<String> = rel
                .components()
                .map(|c| regex_escape(&c.as_os_str().to_string_lossy()))
                .collect();
            push(root, &segments);
        }
    }
    out
}

/// The writable and readable roots that lie strictly inside one of the
/// denied home paths `denied`, and every directory from each such root's
/// parent up to and including the denied path it lies in. A root equal to a
/// denied path, or containing one, is not returned: it stays denied.
fn roots_inside_home_denies(policy: &Resolved, denied: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut ancestors: Vec<PathBuf> = Vec::new();
    for root in policy.writable.iter().chain(&policy.readable) {
        let Some(base) = denied.iter().find(|d| root.starts_with(d) && root != *d) else {
            continue;
        };
        if !roots.contains(root) {
            roots.push(root.clone());
        }
        for dir in root.ancestors().skip(1) {
            if !dir.starts_with(base) {
                break;
            }
            if !ancestors.iter().any(|a| a == dir) {
                ancestors.push(dir.to_path_buf());
            }
        }
    }
    (roots, ancestors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(root: &str, network: Network) -> Resolved {
        Resolved {
            writable: vec![PathBuf::from(root)],
            readable: vec![],
            read_denied: vec![],
            unix_sockets: vec![],
            network,
            home: Some(PathBuf::from("/home/u")),
            ancestor_limit: None,
            cargo_homes: None,
            frozen_git: vec![],
            dependency_reads: vec![],
            hidden: vec![],
            toolchain: super::super::ToolchainReads::default(),
        }
    }

    /// Decision 0094: every frozen git file and the directories above it get
    /// an anchored deny, a `*` segment matching one name; an
    /// include target under a writable root is frozen with its directories
    /// below that root; one outside every root gets no rule.
    #[test]
    fn the_frozen_git_configuration_gets_denies_with_its_directories() {
        let mut r = resolved("/w/repo/.git", Network::Deny);
        r.writable.push(PathBuf::from("/w/tree"));
        r.frozen_git = vec![super::super::FrozenGit {
            common_dir: PathBuf::from("/w/repo/.git"),
            files: vec![
                PathBuf::from("/w/tree/conf/extra.cfg"),
                PathBuf::from("/elsewhere/x.cfg"),
            ],
        }];
        let (text, params) = profile(&r, Path::new("/home/u"));
        let frozen: Vec<String> = params
            .iter()
            .filter(|(k, _)| k.starts_with("FROZEN_GIT_"))
            .map(|(_, v)| v.display().to_string())
            .collect();
        let c = "^/w/repo/\\.git/";
        for want in [
            format!("{c}config$"),
            format!("{c}config\\.lock$"),
            format!("{c}config\\.worktree$"),
            format!("{c}info$"),
            format!("{c}info/attributes$"),
            format!("{c}worktrees$"),
            format!("{c}worktrees/[^/]+$"),
            format!("{c}worktrees/[^/]+/commondir$"),
            format!("{c}worktrees/[^/]+/config\\.worktree$"),
            "^/w/tree/conf$".to_string(),
            "^/w/tree/conf/extra\\.cfg$".to_string(),
        ] {
            assert!(frozen.contains(&want), "missing {want} in {frozen:#?}");
        }
        assert!(!frozen.iter().any(|f| f.contains("elsewhere")));
        assert!(
            !frozen
                .iter()
                .any(|f| f == "^/w/tree$" || f == "^/w/repo/\\.git$")
        );
        assert!(text.contains("(deny file-write*\n  (regex (param \"FROZEN_GIT_0\"))"));
        // After the writable-root allow, so nothing reopens them.
        let allow = text.find("(allow file-write*\n  (subpath").expect("allow");
        assert!(text.find("FROZEN_GIT_0").expect("deny") > allow);
    }

    /// A symlinked credential directory is denied at its target, and the
    /// spelled path stays denied too. Fake fixtures under a temp HOME only.
    #[test]
    fn a_symlinked_home_credential_directory_is_denied_at_its_target() {
        let base = tempfile::tempdir().expect("tempdir");
        let home = kgraf_core::plain_path::canonicalize(base.path()).expect("canonical");
        let target = home.join("elsewhere/keys");
        std::fs::create_dir_all(&target).expect("target");
        std::os::unix::fs::symlink(&target, home.join(".ssh")).expect("link");
        let (text, params) = profile(&resolved("/tmp/w", Network::Deny), &home);
        let denied = |p: &Path| {
            params
                .iter()
                .any(|(k, v)| k.starts_with("DENY_READ_") && v == p)
        };
        assert!(denied(&target), "the link's target is denied");
        assert!(denied(&home.join(".ssh")), "the spelled path stays denied");
        // Both are parameters used by a deny rule, not stray entries.
        for (key, _) in params.iter().filter(|(k, _)| k.starts_with("DENY_READ_")) {
            assert!(text.contains(&format!("(param \"{key}\")")), "{key}");
        }
    }

    #[test]
    fn the_new_credential_files_are_on_the_home_deny_list() {
        for rel in [
            ".config/git/credentials",
            ".vault-token",
            ".pgpass",
            ".terraform.d",
            ".gem/credentials",
            ".codex",
            ".netrc",
            ".m2/settings.xml",
            ".gradle/gradle.properties",
            ".config/containers/auth.json",
            ".huggingface",
            ".cache/huggingface/token",
            ".config/op",
            ".oci",
            ".bundle/config",
            ".claude",
        ] {
            assert!(DENIED_HOME_READS.contains(&rel), "{rel}");
        }
    }

    /// Every deny-list entry reaches the generated profile as a parameter a
    /// `file-read*` deny rule uses, after the blanket read allow.
    #[test]
    fn the_generated_profile_denies_every_home_credential_path() {
        let home = Path::new("/home/u");
        let (text, params) = profile(&resolved("/tmp/w", Network::Deny), home);
        let allow = text.find("(allow file-read*)").expect("read allow");
        for rel in [
            ".m2/settings.xml",
            ".gradle/gradle.properties",
            ".config/containers/auth.json",
            ".huggingface",
            ".cache/huggingface/token",
            ".config/op",
            ".oci",
            ".bundle/config",
            ".claude",
            ".ssh",
            ".pip/pip.conf",
            ".config/pip/pip.conf",
            ".yarnrc",
            ".yarnrc.yml",
            ".config/pypoetry/auth.toml",
            "Library/Application Support/pip/pip.conf",
            "Library/Application Support/pypoetry/auth.toml",
            "Library/Preferences/pnpm/rc",
            ".bunfig.toml",
            ".local/share/uv/credentials",
            ".gradle/enterprise",
            ".gradle/develocity",
            ".cargo/config.toml",
            ".cargo/config",
            ".dockercfg",
            ".s3cfg",
            ".boto",
            ".databrickscfg",
            ".config/rclone",
            ".config/doctl",
            ".config/heroku",
            ".config/glab-cli",
            ".config/helm",
            ".config/stripe",
        ] {
            let (key, _) = params
                .iter()
                .find(|(k, v)| k.starts_with("DENY_READ_") && *v == home.join(rel))
                .unwrap_or_else(|| panic!("{rel} is not a deny parameter"));
            let rule = text
                .find(&format!("(subpath (param \"{key}\"))"))
                .unwrap_or_else(|| panic!("{rel}: no rule uses {key}"));
            assert!(allow < rule, "{rel}: the deny must follow the allow");
        }
    }

    /// A root strictly inside a denied home path is re-allowed after the
    /// home deny, with metadata only for the directories above it; a root
    /// outside one, or one containing a denied path, reopens nothing.
    #[test]
    fn a_root_inside_a_denied_home_path_is_readable_and_nothing_else_is() {
        let home = Path::new("/home/u");
        let mut policy = resolved("/home/u/.claude/p/.kgraf/wt", Network::Deny);
        policy.readable = vec![PathBuf::from("/home/u/.claude/p/.git")];
        let (text, params) = profile(&policy, home);
        let key_of = |prefix: &str, path: &str| {
            params
                .iter()
                .find(|(k, v)| k.starts_with(prefix) && *v == Path::new(path))
                .map(|(k, _)| k.clone())
        };
        let deny = text
            .find("; credential stores, browser profiles, histories")
            .expect("home deny");
        for root in ["/home/u/.claude/p/.kgraf/wt", "/home/u/.claude/p/.git"] {
            let key = key_of("HOME_DENIED_ROOT_", root).expect(root);
            let rule = text
                .find(&format!("(subpath (param \"{key}\"))"))
                .expect(root);
            assert!(deny < rule, "{root}: the re-allow must follow the deny");
        }
        for dir in [
            "/home/u/.claude/p/.kgraf",
            "/home/u/.claude/p",
            "/home/u/.claude",
        ] {
            let key = key_of("HOME_DENIED_ANCESTOR_", dir).expect(dir);
            assert!(
                text.contains(&format!("(literal (param \"{key}\"))")),
                "{dir}"
            );
        }
        assert!(key_of("HOME_DENIED_ANCESTOR_", "/home/u").is_none());
        assert!(
            !text.contains("(subpath (param \"HOME_DENIED_ANCESTOR_"),
            "an ancestor is reopened for metadata only, never as a subtree"
        );

        for outside in ["/tmp/w", "/home/u", "/home/u/.claude"] {
            let (text, params) = profile(&resolved(outside, Network::Deny), home);
            assert!(
                !params.iter().any(|(k, _)| k.starts_with("HOME_DENIED_")),
                "{outside}"
            );
            assert!(
                !text.contains("roots inside a denied home path"),
                "{outside}"
            );
        }
    }

    #[test]
    fn paths_are_parameters_not_profile_text() {
        let root = "/tmp/evil\")(allow file-write* (subpath \"/\")";
        let (text, params) = profile(&resolved(root, Network::Deny), Path::new("/home/u"));
        assert!(!text.contains("evil"));
        assert_eq!(
            params[0],
            ("WRITABLE_ROOT_0".to_string(), PathBuf::from(root))
        );
        assert_eq!(params[1].1, PathBuf::from(root).join(".git"));
    }

    #[test]
    fn only_denied_ancestors_get_metadata_and_only_as_literals() {
        let mut policy = resolved("/r/.kgraf/worktrees/w", Network::Deny);
        policy
            .writable
            .push(PathBuf::from("/r/.kgraf/worktrees/.scratch/w"));
        policy.read_denied.push(PathBuf::from("/r/.kgraf"));
        let (text, params) = profile(&policy, Path::new("/home/u"));
        let mut ancestors: Vec<_> = params
            .iter()
            .filter(|(k, _)| k.starts_with("DENIED_ANCESTOR_"))
            .map(|(_, v)| v.clone())
            .collect();
        ancestors.sort();
        assert_eq!(
            ancestors,
            [
                "/r/.kgraf",
                "/r/.kgraf/worktrees",
                "/r/.kgraf/worktrees/.scratch"
            ]
            .map(PathBuf::from)
        );
        assert!(
            text.contains("(allow file-read-metadata\n  (literal (param \"DENIED_ANCESTOR_0\"))")
        );
        assert!(!text.contains("(subpath (param \"DENIED_ANCESTOR_"));
        // After the state deny, or the deny would shadow it.
        let deny = text.find("; kGraf state").unwrap();
        let allow = text.find("(allow file-read-metadata\n").unwrap();
        assert!(deny < allow);
        // Before the credential deny list, so no ancestor reopens a credential
        // store (a later rule wins in SBPL).
        let credentials = text.find("; credential stores, browser").unwrap();
        assert!(allow < credentials);

        let (text, params) = profile(&resolved("/x/w", Network::Deny), Path::new("/home/u"));
        assert!(!text.contains("DENIED_ANCESTOR_"));
        assert!(
            !params
                .iter()
                .any(|(k, _)| k.starts_with("DENIED_ANCESTOR_"))
        );
    }

    /// Decision 0084: a named checkout is denied, then metadata, the roots
    /// and the cargo files above them re-allowed, in that order (a later rule
    /// wins in SBPL). No cargo file beneath the state directory or in a cargo
    /// home is re-allowed.
    #[test]
    fn a_named_checkout_is_denied_except_its_cargo_files() {
        let mut policy = resolved("/r/.kgraf/worktrees/w", Network::Deny);
        policy.read_denied.push(PathBuf::from("/r/.kgraf"));
        policy.ancestor_limit = Some(PathBuf::from("/r"));
        policy.cargo_homes = Some(vec![PathBuf::from("/r/.cargo")]);
        let (text, params) = profile(&policy, Path::new("/h"));
        let files: Vec<PathBuf> = params
            .iter()
            .filter(|(k, _)| k.starts_with("ANCESTOR_FILE_"))
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(files, [PathBuf::from("/r/Cargo.toml")], "{files:?}");
        let order = [
            "; the checkout",
            "; kGraf state",
            "(allow file-read*\n  (subpath (param \"WRITABLE_ROOT_0\"))",
            "; cargo files above a root",
            "; credential stores, browser",
        ]
        .map(|s| {
            text.find(s)
                .unwrap_or_else(|| panic!("{s} missing:\n{text}"))
        });
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}\n{text}");

        let (text, params) = profile(&resolved("/x/w", Network::Deny), Path::new("/h"));
        assert!(!text.contains("; the checkout"));
        assert!(!params.iter().any(|(k, _)| k.starts_with("ANCESTOR_FILE_")));
    }

    /// A cargo home outside the home folder has its credentials and config
    /// denied by name; one under the home folder is covered by the home list
    /// and adds nothing.
    #[test]
    fn a_cargo_home_outside_the_home_folder_has_its_secret_files_denied() {
        let mut policy = resolved("/x/w", Network::Deny);
        policy.cargo_homes = Some(vec![
            PathBuf::from("/home/u/.cargo"),
            PathBuf::from("/data/cargo"),
        ]);
        let (text, params) = profile(&policy, Path::new("/home/u"));
        let denied: Vec<PathBuf> = params
            .iter()
            .filter(|(k, _)| k.starts_with("DENY_READ_CARGO_HOME_"))
            .map(|(_, v)| v.clone())
            .collect();
        let want: Vec<PathBuf> = CARGO_HOME_SECRET_FILES
            .iter()
            .map(|f| PathBuf::from("/data/cargo").join(f))
            .collect();
        assert_eq!(denied, want);
        let credentials = text.find("; credential stores, browser").unwrap();
        assert!(text[credentials..].contains("DENY_READ_CARGO_HOME_0_0"));
    }

    /// The IP network rules appear for `Network::Allow` and for no other
    /// setting; a denied network gets no IP rule and no resolver policy.
    #[test]
    fn the_ip_network_rules_follow_the_network_setting() {
        let (open, _) = profile(&resolved("/x", Network::Allow), Path::new("/home/u"));
        for rule in [
            "(allow network-outbound (remote ip \"*:*\"))",
            "(allow network-inbound (local ip \"*:*\"))",
            "(allow network-bind (local ip \"*:*\"))",
            MDNS_SOCKET,
        ] {
            assert!(open.contains(rule), "{rule} missing when open");
        }
        let (shut, _) = profile(&resolved("/x", Network::Deny), Path::new("/home/u"));
        for rule in ["remote ip", "local ip", MDNS_SOCKET, NETWORK_POLICY] {
            assert!(!shut.contains(rule), "{rule} present when denied");
        }
    }

    #[test]
    fn outbound_is_never_unqualified() {
        for network in [Network::Allow, Network::Deny] {
            let (text, _) = profile(&resolved("/x", network), Path::new("/home/u"));
            assert!(!text.contains("(allow network-outbound)"), "{network:?}");
            assert!(!text.contains("com.apple.SecurityServer\")"), "{network:?}");
        }
    }
}
