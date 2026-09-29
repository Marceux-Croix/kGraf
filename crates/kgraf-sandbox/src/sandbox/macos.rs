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
    gaps: "reads outside the credential deny list are allowed (source code in other \
           repositories, most of $HOME), apart from the checkout a worktree was made \
           from, whose files other than its cargo files can be stat()ed but not read; \
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
    // Another coding agent's session backups, which can hold the same secrets
    // as its configuration file. The configuration file itself, and every
    // sibling path that starts with its name (a `.backup` or `.tmp.NNNN`
    // copy an editor or the agent itself leaves next to it), is denied below
    // by a regex: a plain `subpath` entry only matches that one literal path
    // and its own descendants, not a sibling file whose name happens to
    // start the same way.
    ".claude/backups",
    // Registry tokens. The sandbox points NPM_CONFIG_USERCONFIG at an empty
    // file in the scratch directory, so npm never looks here.
    ".npmrc",
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
    for (i, rel) in DENIED_HOME_READS.iter().enumerate() {
        let key = format!("DENY_READ_{i}");
        read_denies.push(format!("(subpath (param \"{key}\"))"));
        params.push((key, home.join(rel)));
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
    // After the re-allow, so no root reopens a credential store.
    sections.extend([
        format!(
            "; credential stores, browser profiles, histories\n(deny file-read*\n  {}\n)",
            read_denies.join("\n  ")
        ),
        format!("(allow file-write*\n  {}\n)", write_allows.join("\n  ")),
        socket_rules.join("\n"),
    ]);
    if policy.network == Network::Allow {
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
    sections.push(PREFERENCES_POLICY.to_string());
    sections.push("(deny mach-lookup (xpc-service-name-prefix \"\"))".to_string());
    // Write denies last so no broader allow reopens them.
    sections.extend(write_denies);
    // F_MAKECOMPRESSED (80) and F_TRANSFEREXTENTS (110) mutate files through
    // read-only descriptors, bypassing file-write* (codex).
    sections.push("(deny system-fcntl (fcntl-command 80 110))".to_string());
    (sections.join("\n"), params)
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

    #[test]
    fn outbound_is_never_unqualified() {
        for network in [Network::Allow, Network::Deny] {
            let (text, _) = profile(&resolved("/x", network), Path::new("/home/u"));
            assert!(!text.contains("(allow network-outbound)"), "{network:?}");
            assert!(!text.contains("com.apple.SecurityServer\")"), "{network:?}");
        }
    }
}
