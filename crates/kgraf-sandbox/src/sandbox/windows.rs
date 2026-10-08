//! Windows backend: a write-restricted token, a per-root capability SID
//! admitted by an ACL on each writable root, a Job Object, and a private
//! desktop. No administrator rights, no installer, no service.
//!
//! **Mechanism (decision 0011).** Chosen over
//! AppContainer because the evidence records that AppContainer breaks the
//! shell, Node, MSBuild, git and per-user toolchains without elevation, and
//! it is the toolchain that must run here. Windows Sandbox, WSL2-as-sandbox
//! and Hyperlight are ruled out by the same evidence.
//!
//! - **Token.** `CreateRestrictedToken` with `WRITE_RESTRICTED | LUA_TOKEN |
//!   DISABLE_MAX_PRIVILEGE`. The restricting SIDs are one capability SID per
//!   writable root, a fresh per-spawn SID, the SID of the private desktop the
//!   command runs on, and Everyone. A write succeeds only if both the normal
//!   access check and the check against the restricting SIDs pass. The token
//!   integrity level is then dropped to Low.
//! - **Cross-sandbox isolation.** The token's default DACL grants the user SID
//!   and the per-spawn SID, never the logon SID. Because the per-spawn SID is
//!   unique and is a restricting SID, only the processes of one spawn pass the
//!   restricting write check on each other; a second sandbox (a different
//!   per-spawn SID) cannot open them for write. This closes cross-sandbox
//!   process injection, which a shared logon SID would allow.
//! - **Low integrity (defense in depth).** The
//!   token runs at the Low integrity level and each writable root is labeled
//!   Low. Mandatory Integrity Control then blocks writes up to Medium objects
//!   (the user's files, an Everyone-writable Medium directory) regardless of
//!   any `WRITE_DAC` subtlety, and UIPI blocks `SendInput` and window messages
//!   to the user's desktop. This is defense in depth over the capability
//!   confinement, which stays the primary write boundary. **Unverified:**
//!   whether cargo and the MSVC linker run at Low integrity must be confirmed
//!   on real Windows; if they do not, drop `set_integrity_low` and rely on the
//!   capability SIDs, the per-spawn SID and the private desktop.
//! - **ACL.** Each writable root gets an allow ACE for its capability SID.
//!   Children inherit `DELETE` instead of the root granting
//!   `FILE_DELETE_CHILD`, so the deny ACE on the top-level `.git` entry cannot
//!   be bypassed by deleting it through the parent. The root itself does not
//!   get `DELETE`, so it cannot be removed or moved. Effective write access is
//!   verified with `AccessCheck` under the restricted token, on the root and a
//!   child, so a partial ACL propagation fails loud.
//! - **Reparse and filesystem guards.** No root or `.git` entry that is a
//!   reparse point (a junction or symlink) is touched, so a planted junction
//!   cannot redirect an ACL change onto the user's profile. A root on a volume
//!   without persistent ACLs (FAT, exFAT, some SMB shares) is refused, because
//!   the whole mechanism is ACL-based.
//! - **Job Object.** Kill on job close, no breakaway (no `BREAKAWAY_OK` flag),
//!   every UI restriction (desktop switching, clipboard, global atoms, USER
//!   handles of processes outside the job, system parameters, exit Windows).
//!   The process is created suspended and joins the job before it runs.
//! - **Pipes.** A piped stream's end in this process is opened for
//!   overlapped I/O and the child's end for synchronous I/O, as std's own
//!   child pipes are: std's `ChildStdout` and the others it is handed in
//!   read and write with alertable overlapped calls, which do not work on a
//!   `CreatePipe` handle (see `child_pipe`).
//! - **Private desktop.** Reused per workspace, labeled Low, on the process's
//!   own window station (queried, not hard-coded, so scheduled or service runs
//!   work). Restricted processes must run on a non-default desktop so they
//!   cannot send window messages to unrestricted applications. Its DACL admits
//!   the user SID (the normal access check) and a fresh SID made with the
//!   desktop (the restricting check), and every command that runs on it gets
//!   that SID as a restricting SID. The desktop rights `user32.dll` needs when
//!   it loads (`DESKTOP_CREATEWINDOW`, `DESKTOP_WRITEOBJECTS` and the others in
//!   the desktop's generic write mapping) are write rights, so a desktop that
//!   admits only the user SID fails the restricting check, and every program
//!   that loads `user32.dll` at start (`where`, `whoami`, PowerShell, Node,
//!   the `bin\bash.exe` launcher of Git for Windows through `shell32.dll`)
//!   exits with STATUS_DLL_INIT_FAILED (0xC0000142) before its first
//!   instruction; `cmd.exe` does not load it and runs (the same split is
//!   measured in openai/codex issue 46412; the `desktop_access` tests below
//!   measure the desktop open itself). The desktop SID is
//!   granted on nothing else, so it adds no write anywhere but on that desktop
//!   (decision 0011, amendment "Desktop and environment"). codex grants its
//!   desktop to the logon SID, a restricting SID there; this backend leaves
//!   the logon SID out (see cross-sandbox isolation above).
//!
//! **MSYS2 and Cygwin programs do not start in this sandbox.** The runtime
//! (`msys-2.0.dll`, `cygwin1.dll`) creates its signal pipe while it loads
//! (`sigproc_init`, called from `dll_crt0_0`), with an ACL that grants only
//! the user SID, Administrators and SYSTEM (`sec_user_nih` in
//! `winsup/cygwin/sec/helper.cc`), and then opens it with `GENERIC_WRITE`
//! (`fhandler_pipe::create`). None of those SIDs is a restricting SID here,
//! so the open is refused (`ERROR_ACCESS_DENIED`), the runtime stops with
//! "couldn't create signal pipe, Win32 error 5", and the process exits with
//! STATUS_DLL_INIT_FAILED. The same refusal is reported for codex's
//! unelevated sandbox (openai/codex issues 7031 and 15016). Every fix inside
//! the token makes the user SID (or a group that is granted wherever the
//! user is) a restricting SID, which removes the per-root write confinement;
//! decision 0011 records the options. The `msys_pipe` tests below pin the
//! cause.
//!
//! **Environment.** A command inherits kGraf's environment, minus the
//! credential-shaped names the scrub removes, unless the caller calls
//! `SandboxedCommand::env_clear`. Then it inherits only
//! [`super::WINDOWS_BASE_VARS`], the variables a Windows program needs to start
//! (`SystemRoot`, `PATH`, the profile folders and the others listed there),
//! and the sandbox sets its own again: `TMP` and `TEMP` (the policy's tmp
//! folder) and the git and curl settings (see `git_config_pairs`). With or
//! without a clear, the git entries are numbered from zero and the host's
//! own `GIT_CONFIG_*` entries never reach the command: their values can
//! carry a credential.
//! The profile variables keep their real values: reads are not confined
//! here, writes there are refused by the token, and a tool that cannot find
//! its configuration would fail for no gain.
//!
//! **Partial isolation.** `WRITE_RESTRICTED` makes Windows consult the
//! restricting SIDs for writes only, and Low integrity blocks writes up, not
//! reads. Reads are therefore not confined: credential stores, browser
//! profiles and the Credential Manager API stay readable, and a Low process
//! can open the Medium harness process for `PROCESS_VM_READ`. Network egress
//! is not blocked (only AppContainer or administrator-installed filtering
//! rules can do that), so [`Network::Deny`] is refused here. Checks, gates and
//! the worker's shell ask through [`super::CheckNetwork`] instead, which runs
//! them with the network open, the partial isolation accepted, and a notice
//! (decision 0011, amended 2026-10-06 and 2026-10-07). [`STRENGTH`] reports
//! all of this, and the runner refuses unless the policy opts in to partial
//! isolation (fail loud).
//!
//! **TLS through Schannel does not work in this sandbox.** A program that
//! uses the Windows TLS stack (`curl.exe` from System32, PowerShell
//! `Invoke-WebRequest`, .NET `HttpClient` and `SslStream`, WinHTTP and
//! WinINet, git with `http.sslBackend=schannel`) fails before it sends a
//! byte: `AcquireCredentialsHandle` returns `SEC_E_NO_CREDENTIALS`, even for
//! a client with no certificate and with the network allowed. Schannel's
//! credential calls are served inside LSA, which tells a security package
//! whether its caller is restricted (`SECPKG_CLIENT_INFO::Restricted`), and
//! the outbound credential is refused to a caller whose token has
//! restricting SIDs. Microsoft does not document the refusal; it is
//! measured (openai/codex issue 17459: one restricting SID, Everyone, with
//! or without `WRITE_RESTRICTED`, is enough, and NTLM and Negotiate refuse
//! default credentials the same way). Every token here has restricting
//! SIDs: they are the write boundary. The environment is not the cause
//! (`USERPROFILE`, `APPDATA` and the registry hive are the user's own). The
//! `schannel` tests below measure it per token property: the user's token
//! and a Low copy of it get a credential; a token with a restricting SID
//! does not. A program with its own TLS library
//! (OpenSSL in Git for Windows, Node, Python, Rust with rustls) is not
//! affected. Removing the restricting SIDs would remove the per-root write
//! confinement, so it is not done here (decision 0011, amendment
//! "Schannel"). With the network open, the command's environment steers the
//! tools that can switch: `CURL_SSL_BACKEND=openssl` for the curl of Git for
//! Windows, and `http.sslBackend=openssl` through `GIT_CONFIG_COUNT` for git
//! (see `git_config_pairs`). A tool whose only TLS stack is Schannel
//! (curl.exe from System32, PowerShell, .NET) still fails.
//!
//! **POSIX shells.** A command whose program is `sh` or `bash` runs through
//! Git Bash, with its argv carried in the environment (see
//! [`SHELL_TRAMPOLINE`] and the section above it); with no Git Bash it is not
//! run.
//!
//! **Provenance.** The token construction (restricting-SID order, flags,
//! default DACL, `SeChangeNotifyPrivilege`), the write-ACE mask without
//! `FILE_DELETE_CHILD`, the deny-write mask, the capability SID format and the
//! private-desktop ACL are ported from openai/codex
//! `codex-rs/windows-sandbox-rs/src/{token.rs,acl.rs,cap.rs,desktop.rs,process.rs}`,
//! commit e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0, Copyright 2025
//! OpenAI (see `third_party/codex/UPSTREAM`). Departures: ACL failures are
//! errors instead of being ignored; capability SIDs are derived from the root
//! path instead of persisted in a file; a new desktop per spawn instead of a
//! process-wide cache; no network environment rewrite (it is not isolation).
//!
//! **Status: compile-checked for x86_64-pc-windows-msvc only.** Nothing here
//! has run on Windows. The escape tests in `tests/escape_windows.rs` are
//! written for real Windows and have not run.

// The pure helpers compile on every platform so their unit tests run on the
// development host; only Windows uses them outside tests.
#![cfg_attr(not(windows), allow(dead_code))]

use super::{Backend, IsolationStrength, ReadConfinement};

pub(super) const BACKEND_NAME: &str = "Windows restricted token";

pub(super) const STRENGTH: IsolationStrength = IsolationStrength {
    backend: Backend::WindowsRestrictedToken,
    // Everyone is a restricting SID (codex found tools fail without it), so an
    // object whose DACL grants write to Everyone stays DACL-writable; the Low
    // integrity level then blocks the write unless that object is also labeled
    // Low. writes_confined stays false until both are verified on Windows.
    writes_confined: false,
    reads: ReadConfinement::Unconfined,
    unix_sockets_confined: false,
    network_deniable: false,
    // The deny ACE and the Low integrity level are applied, but neither has
    // been verified on Windows.
    gitlink_protected: false,
    // A deny ACE on a file inside a writable root is bypassed by
    // FILE_DELETE_CHILD on its parent, and none is verified on Windows.
    git_config_freezable: false,
    gaps: "reads are not confined: the Low integrity level blocks writes up, not reads, so \
           %USERPROFILE%\\.ssh, browser profiles, the Credential Manager API and every file \
           the user can read stay readable; a Low process can also open the (Medium) harness \
           process for PROCESS_VM_READ, and the harness holds provider API keys while egress \
           is open, so a read there is exfiltratable; network egress is not blocked; a DACL \
           that grants write to Everyone still needs a Low label to be writable, but a Low, \
           Everyone-writable location (some temp and shared paths) is writable; named pipes \
           and sockets are confined only by their own ACLs and integrity labels; HTTPS \
           through the Windows TLS stack (Schannel: curl.exe from System32, PowerShell, \
           .NET, WinHTTP) fails with SEC_E_NO_CREDENTIALS, because Schannel refuses a client \
           credential to a token with restricting SIDs (git and the curl of Git for Windows \
           are set to OpenSSL, and tools with their own TLS library work); MSYS2 and \
           Cygwin programs (the `sh`, `bash`, `ls` and `grep` of Git Bash) stop at start \
           with STATUS_DLL_INIT_FAILED (0xC0000142), because their runtime opens its own \
           signal pipe for write with an ACL that names only the user, Administrators and \
           SYSTEM, and a write-restricted token passes a write only where a restricting \
           SID is granted; the gitlink deny, the Low label on the writable roots, and \
           cross-sandbox write denial are unverified on Windows",
};

/// The backend name once `kgraf sandbox setup` has run: commands run as a
/// dedicated local sandbox user (decision 0097).
pub(super) const USER_BACKEND_NAME: &str = "Windows sandbox user";

/// What the sandbox user mode enforces. The user's own token has no
/// restricting SIDs, so Schannel TLS and the MSYS2 runtime work; writes are
/// confined by ACL entries for the sandbox user on the writable roots; the
/// owner's profile admits only the owner, SYSTEM and Administrators, so the
/// sandbox user cannot read it; the offline user's network is blocked by
/// firewall rules scoped to its SID. The flags stay false where Windows has
/// not yet confirmed them (the escape tests in `tests/escape_windows.rs`).
pub(super) const USER_STRENGTH: IsolationStrength = IsolationStrength {
    backend: Backend::WindowsSandboxUser,
    // Unverified on Windows.
    writes_confined: false,
    reads: ReadConfinement::CredentialDenyList,
    unix_sockets_confined: false,
    network_deniable: true,
    // The deny ACE on the top-level `.git` entry is written for the sandbox
    // user, but not yet verified on Windows.
    gitlink_protected: false,
    git_config_freezable: false,
    // Each clause is one gap; `user_gaps_name_each_known_gap` pins them.
    gaps: "the sandbox user can read whatever every local user can read (other drives, \
           Program Files, world-readable folders), though not the owner's profile; it can \
           still write where every local user may write below the folders setup closes to \
           it (subfolders of ProgramData and Public that already grant every user write, a \
           drive attached after setup, and TrustedInstaller's folders under the Windows \
           folder, such as System32\\Tasks and System32\\spool\\drivers\\color), and in its \
           own profile, which keeps files from one command to the next; registry keys and \
           named objects every user may write are not closed; DNS lookups go through the \
           system's DNS client service, which the firewall rules do not scope to the \
           sandbox user, so a name lookup can leave the machine; traffic Windows sends on \
           the user's behalf is not proved blocked: a UNC path (\\\\host\\share) makes the \
           kernel's SMB client connect, the WebClient service carries WebDAV, and ICMP \
           (ping) is sent by the system's own network stack, none of which runs as the \
           sandbox user; two commands that run as the same sandbox user (always two in one \
           folder) can write each other's writable folders and open each other's \
           processes, so a command without the network can read the egress proxy's secret \
           from a proxied command of that user and use the proxy while that command runs; \
           unix-domain sockets are confined only by their own ACLs; write confinement, the \
           gitlink deny and the network block are unverified on Windows",
};

#[cfg(test)]
mod user_gaps_tests {
    use super::USER_STRENGTH;

    #[test]
    fn user_gaps_name_each_known_gap() {
        let gaps = USER_STRENGTH.gaps;
        assert!(!gaps.contains("  "), "a run of spaces: {gaps}");
        assert!(!gaps.contains('\n'), "a line break: {gaps}");
        for needle in [
            "not the owner's profile",
            "below the folders setup closes",
            "in its own profile",
            "registry keys",
            "DNS",
            "UNC path",
            "WebDAV",
            "ICMP",
            "proxy's secret",
            "unverified on Windows",
        ] {
            assert!(gaps.contains(needle), "{needle:?} is missing: {gaps}");
        }
        // Commands in different folders no longer share one user.
        assert!(!gaps.contains("run at the same time run as the same user"));
    }
}

/// The TLS library tools are steered to inside this sandbox. The Windows
/// stack (Schannel) cannot run under the sandbox token (see the module
/// header); Git for Windows ships OpenSSL beside it.
pub(super) const TLS_BACKEND: &str = "openssl";

/// The variable the multi-backend curl of Git for Windows (and git's own
/// libcurl) reads to pick its TLS backend.
pub(super) const CURL_SSL_BACKEND_VAR: &str = "CURL_SSL_BACKEND";

/// The `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` pairs the sandbox gives a
/// command, numbered from zero, and their count. The host's own entries are
/// not inherited (`super::is_git_config_entry_var`). The first entry resets
/// the credential helper list, so git does not ask the Credential Manager.
/// With the network open, a second selects OpenSSL for git's HTTPS
/// (`http.sslBackend`), because Schannel cannot run under the sandbox token.
/// Then come the line-ending settings kGraf's own git uses
/// (`kgraf_core::host_git::LINE_ENDINGS`). None touches
/// `GIT_CONFIG_NOSYSTEM` or `GIT_CONFIG_GLOBAL`, which keep their effect.
pub(super) fn git_config_pairs(network_open: bool) -> Vec<(&'static str, &'static str)> {
    let mut pairs: Vec<(&str, &str)> = vec![("credential.helper", "")];
    if network_open {
        pairs.push(("http.sslBackend", TLS_BACKEND));
    }
    pairs.extend_from_slice(kgraf_core::host_git::LINE_ENDINGS);
    pairs
}

/// Every variable the sandbox sets for the tools on Windows: the
/// [`git_config_pairs`] as `GIT_CONFIG_*` entries from index zero and their
/// `GIT_CONFIG_COUNT`, and, with the network open, [`CURL_SSL_BACKEND_VAR`].
pub(super) fn tool_env(network_open: bool) -> Vec<(String, String)> {
    let mut vars = super::git_config_vars(&git_config_pairs(network_open));
    if network_open {
        vars.push((CURL_SSL_BACKEND_VAR.to_owned(), TLS_BACKEND.to_owned()));
    }
    vars
}

/// What `SandboxedCommand::env_clear` sets again on Windows: [`tool_env`]
/// (numbered from zero, as without a clear), and `TMP` and
/// `TEMP` set to the policy's tmp folder: without them Windows programs
/// fall back to folders the token cannot write.
pub(super) fn kept_on_clear(
    network_open: bool,
    tmp: Option<&std::path::Path>,
) -> Vec<(String, std::ffi::OsString)> {
    let mut kept: Vec<(String, std::ffi::OsString)> = tool_env(network_open)
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect();
    if let Some(tmp) = tmp {
        kept.push(("TMP".to_owned(), tmp.as_os_str().to_owned()));
        kept.push(("TEMP".to_owned(), tmp.as_os_str().to_owned()));
    }
    kept
}

/// The capability SID for one writable root. Deterministic, so repeated runs
/// reuse one ACE instead of adding one per run (and re-propagating it through
/// the tree). Shape `S-1-5-21-a-b-c-d` follows codex `cap.rs`; codex draws
/// random values and persists them, which needs a state file. A SID is not a
/// secret: a process cannot add a SID to its own token without privilege.
pub(super) fn capability_sid_string(root_key: &str) -> String {
    let key = root_key.to_lowercase();
    let h1 = fnv1a64(0xcbf2_9ce4_8422_2325, key.as_bytes());
    let h2 = fnv1a64(0x6c62_272e_07bb_0142, key.as_bytes());
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        (h1 >> 32) as u32,
        h1 as u32,
        (h2 >> 32) as u32,
        h2 as u32
    )
}

/// The domain separator keeps the product's former name (decision 0062): it
/// is part of every capability SID already granted in an ACL on a user's
/// disk, and a new separator would give each root a second ACE with nothing
/// gained. It is an identifier, never shown.
fn fnv1a64(seed: u64, bytes: &[u8]) -> u64 {
    b"haustoria-sandbox-cap-v1\0"
        .iter()
        .chain(bytes)
        .fold(seed, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// The first link beneath `root` (a junction or symbolic link on Windows,
/// where std's `is_symlink` is true for both) whose target lies outside
/// `root` or cannot be resolved, with that target; `None` when there is
/// none. Links are never entered.
///
/// Writing an inheritable entry on a folder makes Windows write it on every
/// object that already exists below, and whether that walk enters a junction
/// is not documented. An earlier command could have planted one pointing at
/// the owner's files, so the first grant for a SID on a tree is refused while
/// such a link exists. A link whose target stays inside `root` is allowed:
/// the walk would reach only what the grant covers anyway (pnpm lays out
/// `node_modules` with such junctions). Bounded by the tree's size; it runs
/// only when the entry is absent, and that write walks the same tree.
///
/// A command that runs at the same time could plant a link between this walk
/// and the write; the first grant for a SID on a folder happens before any
/// command of that SID runs there, which leaves only commands of another
/// sandbox user in the same folder.
pub(super) fn first_outside_link(
    root: &std::path::Path,
) -> std::io::Result<Option<(std::path::PathBuf, Option<std::path::PathBuf>)>> {
    let real_root = kgraf_core::plain_path::canonicalize(root)?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                match kgraf_core::plain_path::canonicalize(entry.path()) {
                    Ok(target) if target.starts_with(&real_root) => {}
                    Ok(target) => return Ok(Some((entry.path(), Some(target)))),
                    Err(_) => return Ok(Some((entry.path(), None))),
                }
            } else if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(None)
}

/// The refusal [`first_outside_link`] leads to.
pub(super) fn outside_link_refusal(
    root: &std::path::Path,
    link: &std::path::Path,
    target: Option<&std::path::Path>,
) -> String {
    let target = target.map_or_else(
        || "a target that cannot be resolved".to_owned(),
        |t| t.display().to_string(),
    );
    format!(
        "{} holds the link {} to {target}, outside it. Writing the sandbox user's entry on \
         {} could carry it through the link onto {target}, so nothing was written. Remove \
         the link, then run the command again.",
        root.display(),
        link.display(),
        root.display()
    )
}

#[cfg(all(test, unix))]
mod outside_link_tests {
    use super::first_outside_link;

    #[test]
    fn a_link_out_of_the_tree_is_found_and_one_inside_is_not() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let deep = root.path().join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.txt"), "x").unwrap();
        // Inside: allowed, and not entered.
        std::os::unix::fs::symlink(root.path().join("a"), deep.join("up")).unwrap();
        assert_eq!(first_outside_link(root.path()).unwrap(), None);
        // Outside, two levels down.
        let link = deep.join("out");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let (found, target) = first_outside_link(root.path()).unwrap().unwrap();
        assert_eq!(found, link);
        assert_eq!(
            target.unwrap(),
            kgraf_core::plain_path::canonicalize(outside.path()).unwrap()
        );
        // A dangling link cannot be judged, so it is refused.
        std::fs::remove_file(&link).unwrap();
        let dangling = root.path().join("dangling");
        std::os::unix::fs::symlink(outside.path().join("gone"), &dangling).unwrap();
        assert_eq!(
            first_outside_link(root.path()).unwrap(),
            Some((dangling, None))
        );
    }
}

/// `true` if the program is a batch file. `CreateProcess` runs those through
/// `cmd.exe`, whose argument parsing differs from the MSVC rules (the
/// BatBadBut class of injection); callers must invoke `cmd.exe` themselves.
/// The name prefix of the directories the ACL check creates in a writable
/// root, followed by `<pid>-<n>`.
const ACL_PROBE_PREFIX: &str = ".kgraf-acl-probe-";

/// The process id in an ACL probe directory's name, or `None` for any other
/// name.
fn acl_probe_pid(name: &str) -> Option<u32> {
    let (pid, n) = name.strip_prefix(ACL_PROBE_PREFIX)?.split_once('-')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

pub(super) fn is_batch_file(program: &str) -> bool {
    let lower = program.to_ascii_lowercase();
    lower.ends_with(".bat") || lower.ends_with(".cmd")
}

/// Whether `name` is one of [`super::WINDOWS_BASE_VARS`], compared without case
/// as Windows compares variable names.
pub(super) fn is_base_var(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| {
        super::WINDOWS_BASE_VARS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(n))
    })
}

/// What a command inherits from `parent` (this process's environment): all
/// of it, or, once its environment was cleared, only
/// [`super::WINDOWS_BASE_VARS`].
/// Variables the sandbox or the caller set on the command are applied over
/// this ([`env_block`]).
pub(super) fn inherited_vars<I>(
    parent: I,
    inherit_all: bool,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    parent
        .into_iter()
        .filter(|(name, _)| inherit_all || is_base_var(name))
        .collect()
}

/// An environment variable as UTF-16 name and value.
pub(super) type WideVar = (Vec<u16>, Vec<u16>);

/// Merge overrides into the inherited environment (names compare without
/// case, as Windows does) and encode a `CREATE_UNICODE_ENVIRONMENT` block:
/// `NAME=VALUE\0` entries sorted by upper-cased name, then a final `\0`.
pub(super) fn env_block(
    inherited: Vec<WideVar>,
    overrides: Vec<(Vec<u16>, Option<Vec<u16>>)>,
) -> Vec<u16> {
    let upper = |name: &[u16]| String::from_utf16_lossy(name).to_uppercase();
    let mut vars: std::collections::BTreeMap<String, WideVar> = inherited
        .into_iter()
        .map(|(name, value)| (upper(&name), (name, value)))
        .collect();
    for (name, value) in overrides {
        let key = upper(&name);
        match value {
            Some(value) => {
                vars.insert(key, (name, value));
            }
            None => {
                vars.remove(&key);
            }
        }
    }
    let mut block = Vec::new();
    for (name, value) in vars.into_values() {
        block.extend_from_slice(&name);
        block.push(u16::from(b'='));
        block.extend_from_slice(&value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

// ---- POSIX shells through Git Bash ------------------------------------------
//
// A command whose program is `sh` or `bash` (a worker's `["sh", "-c", line]`,
// a gate's `sh -c`, a contract check's argv) runs on Windows through Git Bash,
// the `bin\bash.exe` of Git for Windows. Chosen because the command policy reads
// an `sh -c` script as POSIX shell text (its commands, redirects and grants), so
// the text must run in a POSIX shell: PowerShell or cmd would run a different
// language than the one that was judged. Claude Code makes the same choice
// (Git Bash for its Bash tool, `CLAUDE_CODE_GIT_BASH_PATH` to name it); Codex
// defaults to PowerShell on Windows, which fits a model told to write
// PowerShell, not kGraf's POSIX-judged `sh -c`. With no Git Bash the spawn
// fails and says how to install or name one; it never falls back to another
// shell. A model can still run `powershell -Command` or `cmd /c` as a program.
//
// **Why the arguments go through the environment.** Git Bash is an MSYS2
// program, and the MSYS2 runtime splits a command line from a Windows parent
// with its own rules, not the MSVC rules `win_command_line::append_arg` writes
// for (msys2-runtime `winsup/cygwin/dcrt0.cc`, `build_argv`, `quoted`,
// `globify`): single quotes
// quote, `\\` inside quotes becomes `\`, a quote escaped as `\"` can end a
// quoted run early and split the word, unquoted words are globbed with braces
// and `~`, and a word starting with `@` names a file of more arguments. A
// script holds all of these. So the command line carries one fixed script,
// [`SHELL_TRAMPOLINE`], with no quote or backslash in it, that both parsers
// read the same way, and the real argv goes in [`SHELL_COMMAND_VAR`],
// single-quoted for the shell. The trampoline evaluates it with no field
// splitting and no globbing, and that `exec`s `/usr/bin/sh` or
// `/usr/bin/bash` with the exact argv. MSYS2 passes an argv to an MSYS2 child
// without a command line, so nothing is parsed twice.

/// The variable that carries a POSIX shell's argv, single-quoted, to Git Bash.
/// The evaluated text unsets it before the shell runs, so the command does
/// not see it.
pub(super) const SHELL_COMMAND_VAR: &str = "KGRAF_SHELL_COMMAND";

/// The script Git Bash gets on its command line: evaluate
/// [`SHELL_COMMAND_VAR`] as one word. `IFS=` stops field splitting and
/// `set -f` stops globbing of the expansion, so the text reaches `eval`
/// byte for byte. It holds no quote and no backslash, the only characters
/// that the MSVC and the MSYS2 rules read differently inside a quoted word.
pub(super) const SHELL_TRAMPOLINE: &str = "IFS=;set -f;eval $KGRAF_SHELL_COMMAND";

/// `"sh"` or `"bash"` when `program` names a POSIX shell that runs through
/// Git Bash: the bare name (with or without `.exe`, any case) or its usual
/// POSIX path. A Windows path to a particular `bash.exe` is run as given.
/// A bare `bash` is never looked up on `PATH`: `C:\Windows\System32\bash.exe`
/// is the WSL launcher, which runs the command in a Linux machine outside
/// this sandbox.
pub(super) fn posix_shell(program: &str) -> Option<&'static str> {
    let lower = program.to_ascii_lowercase();
    let name = match lower.as_str() {
        "/bin/sh" | "/usr/bin/sh" => return Some("sh"),
        "/bin/bash" | "/usr/bin/bash" => return Some("bash"),
        other => other.strip_suffix(".exe").unwrap_or(other),
    };
    match name {
        "sh" => Some("sh"),
        "bash" => Some("bash"),
        _ => None,
    }
}

/// `arg` as one POSIX shell word: single-quoted, each `'` written `'\''`.
/// Every byte is literal inside single quotes, so no argument can change
/// the command it is part of.
pub(super) fn posix_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The text the trampoline evaluates: unset the carrier variable, restore
/// globbing and the default field separators (an `exec`ed shell starts with
/// its own, but a `SHELLOPTS` the parent exported would carry `noglob` on),
/// then `exec` `shell` (a path inside Git Bash) with `args`, each quoted.
pub(super) fn shell_command_text(shell: &str, args: &[&str]) -> String {
    let mut text = format!(
        "unset {SHELL_COMMAND_VAR}; set +f; IFS=$' \\t\\n'; exec {}",
        posix_quote(shell)
    );
    for arg in args {
        text.push(' ');
        text.push_str(&posix_quote(arg));
    }
    text
}

/// Longest value Windows allows one environment variable to hold, in UTF-16
/// units, without its terminating NUL.
const MAX_ENV_VALUE: usize = 32_766;

/// How a POSIX shell command starts on Windows: `program` (Git Bash) with
/// `args`, and [`SHELL_COMMAND_VAR`] set to `command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellLaunch {
    pub program: std::path::PathBuf,
    pub args: [&'static str; 2],
    pub command: String,
}

/// The launch of `shell` (`"sh"` or `"bash"`, from [`posix_shell`]) with
/// `args` under the Git Bash at `bash`. Refused, with the reason, for an
/// argument that is not Unicode or holds a NUL (an environment value cannot
/// carry either), and for a command longer than one variable may hold.
pub(super) fn shell_launch(
    shell: &str,
    args: &[&std::ffi::OsStr],
    bash: std::path::PathBuf,
) -> Result<ShellLaunch, String> {
    let mut words = Vec::with_capacity(args.len());
    for arg in args {
        let word = arg
            .to_str()
            .ok_or_else(|| format!("an argument to `{shell}` is not valid Unicode"))?;
        if word.contains('\0') {
            return Err(format!("an argument to `{shell}` holds a NUL character"));
        }
        words.push(word);
    }
    let command = shell_command_text(&format!("/usr/bin/{shell}"), &words);
    let units = command.encode_utf16().count();
    if units > MAX_ENV_VALUE {
        return Err(format!(
            "the `{shell}` command is {units} UTF-16 units long; Windows passes at most \
             {MAX_ENV_VALUE} in one environment variable"
        ));
    }
    Ok(ShellLaunch {
        program: bash,
        args: ["-c", SHELL_TRAMPOLINE],
        command,
    })
}

/// For each hidden folder (`SandboxPolicy::hide`), the capabilities whose
/// writes in it are denied: those of the writable roots that hold it. A
/// writable root beneath the folder (a package store inside `.kgraf`) keeps
/// its own capability undenied, and the explicit grant on that root comes
/// before the deny it inherits from the folder (Windows orders explicit
/// entries, then inherited ones nearest first), so writes there still pass.
/// `roots` and `caps` are parallel; paths compare as given, after the
/// caller has stripped verbatim prefixes from all of them.
pub(super) fn hidden_denials<'a, S>(
    hidden: &[super::Hidden],
    roots: &[std::path::PathBuf],
    caps: &'a [S],
    normalize: impl Fn(&std::path::Path) -> std::path::PathBuf,
) -> Vec<(std::path::PathBuf, Vec<&'a S>)> {
    hidden
        .iter()
        .map(|h| {
            let dir = normalize(&h.dir);
            let sids = roots
                .iter()
                .zip(caps)
                .filter(|(root, _)| dir.starts_with(root) && **root != dir)
                .map(|(_, sid)| sid)
                .collect();
            (dir, sids)
        })
        .collect()
}

#[cfg(windows)]
pub use imp::SandboxedChild;
#[cfg(windows)]
pub(crate) use imp::{AttributeList, revoke};
#[cfg(windows)]
pub(super) use imp::{
    Prepared, default_stdio, is_partial, output, prepare, prepare_root, spawn, status, strength,
    worker_slot,
};

#[cfg(windows)]
mod imp {
    use std::ffi::{OsStr, OsString, c_void};
    use std::io::{self, Read};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Output};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_SID,
        ERROR_PIPE_BUSY, ERROR_SUCCESS, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, GetLastError,
        HANDLE, HANDLE_FLAG_INHERIT, HLOCAL, INVALID_HANDLE_VALUE, LUID, LocalFree,
        SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        GetNamedSecurityInfoW, REVOKE_ACCESS, SE_FILE_OBJECT, SE_WINDOW_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AccessCheck, AddMandatoryAce,
        AdjustTokenPrivileges, CONTAINER_INHERIT_ACE, CopySid, CreateRestrictedToken,
        DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, EqualSid,
        GENERIC_MAPPING, GROUP_SECURITY_INFORMATION, GetAce, GetLengthSid, GetTokenInformation,
        INHERIT_ONLY_ACE, InitializeAcl, IsValidSid, LABEL_SECURITY_INFORMATION, LUA_TOKEN,
        LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, NO_INHERITANCE, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, PSID,
        SE_PRIVILEGE_ENABLED, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, SecurityImpersonation,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_IMPERSONATE,
        TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl,
        TokenGroups, TokenImpersonation, TokenIntegrityLevel, TokenUser, WRITE_RESTRICTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_DELETE_CHILD,
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_EXECUTE,
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
        GetVolumeInformationW, OPEN_EXISTING, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
        READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, CreatePipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, CreateDesktopW, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE,
        DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READ_CONTROL,
        DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS, GetProcessWindowStation,
        GetUserObjectInformationW, HDESK, UOI_NAME,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, FILE_PERSISTENT_ACLS,
        JOB_OBJECT_UILIMIT_ALL, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID,
        SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    use super::super::{IsolationStrength, Network, Redirect, Resolved};
    use super::{
        BACKEND_NAME, SHELL_COMMAND_VAR, STRENGTH, ShellLaunch, USER_BACKEND_NAME, USER_STRENGTH,
        capability_sid_string, env_block, inherited_vars, is_batch_file, posix_shell, shell_launch,
    };
    use crate::error::SandboxError;
    use crate::sandbox_user::PROXY_PORTS;
    use crate::sandbox_user::runner::Request;
    use crate::sandbox_user::slots::{COMMAND_WAIT, Lease, Pool};
    use crate::sandbox_user::windows::launch;
    use crate::sandbox_user::windows::state::{self, LoadedUser, Users};
    use std::sync::Arc;
    // Shared with the command policy's check of a `cmd /c` or PowerShell
    // script, so it is built on every platform.
    use crate::win_command_line::command_line;

    // ---- errors -------------------------------------------------------------

    fn last_error(step: &'static str) -> SandboxError {
        SandboxError::PlatformSetup {
            step,
            source: io::Error::last_os_error(),
        }
    }

    fn win32(step: &'static str, code: u32) -> SandboxError {
        SandboxError::PlatformSetup {
            step,
            source: io::Error::from_raw_os_error(code as i32),
        }
    }

    fn acl_error(path: &Path, code: u32) -> SandboxError {
        SandboxError::Io {
            path: path.to_path_buf(),
            source: io::Error::from_raw_os_error(code as i32),
        }
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Take ownership of a handle a Win32 call returned, or report the call's
    /// error. Null and `INVALID_HANDLE_VALUE` both mean failure here.
    fn owned(h: HANDLE, step: &'static str) -> Result<OwnedHandle, SandboxError> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(last_error(step));
        }
        // SAFETY: `h` is a valid handle just returned to this process, and
        // nothing else owns it.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// Memory the system allocated with `LocalAlloc` (SIDs, ACLs, security
    /// descriptors), freed on drop.
    struct Local(*mut c_void);

    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a Win32 call documented to
                // allocate with LocalAlloc, and is freed once.
                unsafe { LocalFree(self.0 as HLOCAL) };
            }
        }
    }

    // ---- SIDs ---------------------------------------------------------------

    /// An owned SID in a `u32` buffer, so the subauthority array is aligned.
    #[derive(Debug, Clone)]
    pub(in crate::sandbox) struct Sid(Vec<u32>);

    impl Sid {
        fn from_string(s: &str) -> Result<Self, SandboxError> {
            let text = wide(OsStr::new(s));
            let mut psid: PSID = ptr::null_mut();
            // SAFETY: `text` is NUL-terminated; on success the system writes a
            // LocalAlloc'd SID pointer, released by `Local`.
            if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut psid) } == 0 {
                return Err(last_error("ConvertStringSidToSidW"));
            }
            let _guard = Local(psid);
            // SAFETY: `psid` is a valid SID returned above.
            unsafe { Self::copy_from(psid) }
        }

        /// # Safety
        /// `psid` must point to a valid SID.
        unsafe fn copy_from(psid: PSID) -> Result<Self, SandboxError> {
            // SAFETY: caller guarantees a valid SID.
            if unsafe { IsValidSid(psid) } == 0 {
                return Err(win32("IsValidSid", ERROR_INVALID_SID));
            }
            // SAFETY: valid SID.
            let len = unsafe { GetLengthSid(psid) };
            let mut buf = vec![0u32; (len as usize).div_ceil(4)];
            // SAFETY: `buf` holds at least `len` bytes.
            if unsafe { CopySid(len, buf.as_mut_ptr().cast(), psid) } == 0 {
                return Err(last_error("CopySid"));
            }
            Ok(Self(buf))
        }

        fn as_psid(&self) -> PSID {
            self.0.as_ptr() as PSID
        }
    }

    fn trustee(sid: &Sid) -> TRUSTEE_W {
        TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_psid().cast(),
        }
    }

    fn explicit(sid: &Sid, mask: u32, mode: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: mode,
            grfInheritance: inheritance,
            Trustee: trustee(sid),
        }
    }

    // ---- ACLs on writable roots ----------------------------------------------

    /// Read, write and execute without `DELETE`: applied to the root itself.
    const ROOT_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    /// Inherited by everything below the root. `DELETE` on each child instead
    /// of `FILE_DELETE_CHILD` on the parent, which would bypass a deny ACE on
    /// a protected child (codex `acl.rs`, `WRITE_ALLOW_MASK`).
    const TREE_MASK: u32 = ROOT_MASK | DELETE;
    const TREE_INHERIT: u32 = INHERIT_ONLY_ACE | OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    /// What the gitlink deny removes. Not `FILE_GENERIC_WRITE`: that includes
    /// `SYNCHRONIZE` and `READ_CONTROL`, which reads need.
    const DENY_WRITE_MASK: u32 = FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD
        | WRITE_DAC
        | WRITE_OWNER;

    /// A file object's DACL and the descriptor that owns it.
    struct Dacl {
        acl: *mut ACL,
        _sd: Local,
    }

    fn read_dacl(path: &Path) -> Result<Dacl, SandboxError> {
        let name = wide(path.as_os_str());
        let mut acl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; outputs are written on success and
        // the descriptor is freed by `Local`.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
                ptr::null_mut(),
                &mut sd,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        Ok(Dacl {
            acl,
            _sd: Local(sd),
        })
    }

    /// Whether `dacl` holds an explicit (not inherited) ACE of `ace_type` for
    /// `sid` covering `mask`, with exactly the inheritance flags `flags`.
    fn has_ace(dacl: &Dacl, ace_type: u32, sid: &Sid, mask: u32, flags: u32) -> bool {
        if dacl.acl.is_null() {
            return false;
        }
        const INHERITANCE_BITS: u32 = 0x0F; // OI | CI | NP | IO
        const INHERITED_ACE: u32 = 0x10;
        // SAFETY: `acl` points into the descriptor `dacl` owns.
        let count = unsafe { (*dacl.acl).AceCount };
        (0..u32::from(count)).any(|i| {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `i` is below the ACE count of a valid ACL.
            if unsafe { GetAce(dacl.acl, i, &mut ace) } == 0 {
                return false;
            }
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            if u32::from(header.AceType) != ace_type
                || u32::from(header.AceFlags) & INHERITED_ACE != 0
                || u32::from(header.AceFlags) & INHERITANCE_BITS != flags
            {
                return false;
            }
            // SAFETY: allowed and denied ACEs share the ACCESS_ALLOWED_ACE
            // layout; the SID starts at `SidStart`.
            let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
            // SAFETY: both are valid SIDs.
            body.Mask & mask == mask && unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0
        })
    }

    /// Merge `entries` into the DACL of `path` and write it back. Writing a
    /// directory's DACL propagates inheritable ACEs to every existing child,
    /// so callers skip this when the ACEs are already present.
    fn apply(path: &Path, dacl: &Dacl, entries: &[EXPLICIT_ACCESS_W]) -> Result<(), SandboxError> {
        let mut new_acl: *mut ACL = ptr::null_mut();
        // SAFETY: `entries` is a valid slice; the old ACL is valid or null;
        // the new ACL is LocalAlloc'd and freed by `Local`.
        let code = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                dacl.acl,
                &mut new_acl,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        let _guard = Local(new_acl.cast());
        let name = wide(path.as_os_str());
        // SAFETY: `name` is NUL-terminated; `new_acl` is a valid ACL.
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new_acl,
                ptr::null(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        Ok(())
    }

    /// Refuse to write an inheritable entry on the tree `root` while a link
    /// beneath it leads outside it ([`super::first_outside_link`]): the write
    /// propagates to what already exists below, and could follow the link.
    fn refuse_outside_links(root: &Path) -> Result<(), SandboxError> {
        match super::first_outside_link(root) {
            Ok(None) => Ok(()),
            Ok(Some((link, target))) => Err(SandboxError::PlatformSetup {
                step: "check a tree for links before granting on it",
                source: io::Error::other(super::outside_link_refusal(
                    root,
                    &link,
                    target.as_deref(),
                )),
            }),
            Err(source) => Err(SandboxError::Io {
                path: root.to_path_buf(),
                source,
            }),
        }
    }

    /// Admit `sid` to write beneath `root`. The owner of an object may always
    /// rewrite its DACL, so this needs no administrator rights for a
    /// user-owned worktree. Verified by reading the DACL back (fail loud).
    /// The first grant for `sid` is refused while a link in the tree leads
    /// outside it ([`refuse_outside_links`]).
    fn grant_writable_root(root: &Path, sid: &Sid) -> Result<(), SandboxError> {
        let present = |d: &Dacl| {
            has_ace(d, ACCESS_ALLOWED_ACE_TYPE, sid, ROOT_MASK, NO_INHERITANCE)
                && has_ace(d, ACCESS_ALLOWED_ACE_TYPE, sid, TREE_MASK, TREE_INHERIT)
        };
        let dacl = read_dacl(root)?;
        if present(&dacl) {
            return Ok(());
        }
        refuse_outside_links(root)?;
        apply(
            root,
            &dacl,
            &[
                explicit(sid, ROOT_MASK, GRANT_ACCESS, NO_INHERITANCE),
                explicit(sid, TREE_MASK, GRANT_ACCESS, TREE_INHERIT),
            ],
        )?;
        if !present(&read_dacl(root)?) {
            return Err(SandboxError::PlatformSetup {
                step: "verify writable-root ACEs",
                source: io::Error::other(format!(
                    "the allow ACEs for {} are missing after SetNamedSecurityInfoW",
                    root.display()
                )),
            });
        }
        Ok(())
    }

    // ---- ACLs for the sandbox user (decision 0097) ---------------------------

    /// Read and run: what the sandbox user gets on the folders it reads.
    const READ_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    /// What a hidden folder denies the sandbox user: reads and writes. Deny
    /// on read takes effect here, unlike under the write-restricted token,
    /// whose restricting SIDs are consulted for writes only.
    const DENY_ALL_MASK: u32 = READ_MASK | DENY_WRITE_MASK;

    /// Admit `sid` to read and run `path` and, for a folder, everything
    /// below it. Written only while absent, as [`grant_writable_root`] is.
    /// True when it was written now. A reparse point is refused first, so
    /// the entry never lands on a link's target (decision 0083).
    fn grant_read(path: &Path, sid: &Sid) -> Result<bool, SandboxError> {
        refuse_reparse(path)?;
        let flags = if path.is_dir() {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            NO_INHERITANCE
        };
        let dacl = read_dacl(path)?;
        if has_ace(&dacl, ACCESS_ALLOWED_ACE_TYPE, sid, READ_MASK, flags) {
            return Ok(false);
        }
        if path.is_dir() {
            refuse_outside_links(path)?;
        }
        apply(
            path,
            &dacl,
            &[explicit(sid, READ_MASK, GRANT_ACCESS, flags)],
        )?;
        if !has_ace(
            &read_dacl(path)?,
            ACCESS_ALLOWED_ACE_TYPE,
            sid,
            READ_MASK,
            flags,
        ) {
            return Err(SandboxError::PlatformSetup {
                step: "verify a read grant for the sandbox user",
                source: io::Error::other(format!(
                    "the read entry for {} is missing after SetNamedSecurityInfoW",
                    path.display()
                )),
            });
        }
        Ok(true)
    }

    /// A grant on a tree that holds a junction to a folder outside it.
    /// Windows only; compile-checked on the development host.
    #[cfg(test)]
    mod propagation {
        use super::*;

        /// A SID no token holds, so the entries change no one's access.
        const SID: &str = "S-1-5-21-1-2-3-4";

        fn junction(link: &Path, target: &Path) {
            let status = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .status()
                .expect("run mklink");
            assert!(status.success(), "could not make the junction");
        }

        /// Whether any entry of `path`'s list, inherited or not, names `sid`.
        fn mentions(path: &Path, sid: &Sid) -> bool {
            let dacl = read_dacl(path).unwrap();
            if dacl.acl.is_null() {
                return false;
            }
            // SAFETY: a valid list `dacl` owns.
            let count = unsafe { (*dacl.acl).AceCount };
            (0..u32::from(count)).any(|i| {
                let mut ace: *mut c_void = ptr::null_mut();
                // SAFETY: `i` is below the entry count.
                if unsafe { GetAce(dacl.acl, i, &mut ace) } == 0 {
                    return false;
                }
                // SAFETY: allow and deny entries share this layout; a file
                // list holds no other kind.
                let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
                let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
                // SAFETY: both are valid SIDs.
                unsafe { EqualSid(ace_sid, sid.as_psid()) != 0 }
            })
        }

        #[test]
        fn a_first_grant_on_a_tree_with_a_junction_out_of_it_is_refused() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("owner.txt"), "x").unwrap();
            std::fs::create_dir_all(root.path().join("deep")).unwrap();
            junction(&root.path().join("deep").join("jn"), outside.path());
            let err = grant_writable_root(root.path(), &sid).unwrap_err();
            assert!(err.to_string().contains("outside it"), "{err}");
            assert!(!mentions(root.path(), &sid), "nothing written on the root");
            assert!(!mentions(outside.path(), &sid));
            assert!(!mentions(&outside.path().join("owner.txt"), &sid));
            let err = grant_read(root.path(), &sid).unwrap_err();
            assert!(err.to_string().contains("outside it"), "{err}");
            assert!(!mentions(outside.path(), &sid));
        }

        #[test]
        fn a_junction_that_stays_in_the_tree_does_not_stop_the_grant() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("store")).unwrap();
            junction(&root.path().join("jn"), &root.path().join("store"));
            grant_writable_root(root.path(), &sid).unwrap();
            assert!(mentions(root.path(), &sid));
        }

        /// What the guard above prevents, measured: whether writing an
        /// inheritable entry on a folder carries it through a junction onto
        /// the junction's target. The guard holds either way; this line tells
        /// the run's reader which. Search the output for `MEASURE`.
        #[test]
        fn measure_whether_propagation_follows_a_junction() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("owner.txt"), "x").unwrap();
            junction(&root.path().join("jn"), outside.path());
            let dacl = read_dacl(root.path()).unwrap();
            apply(
                root.path(),
                &dacl,
                &[explicit(&sid, TREE_MASK, GRANT_ACCESS, TREE_INHERIT)],
            )
            .unwrap();
            println!(
                "MEASURE grant-propagation-follows-junction: target folder {}, file in it {}",
                mentions(outside.path(), &sid),
                mentions(&outside.path().join("owner.txt"), &sid)
            );
        }
    }

    /// Deny `sid` every read and write beneath the hidden folder `dir`. A
    /// writable root beneath it keeps its own explicit grant, which precedes
    /// the inherited deny.
    fn deny_all(dir: &Path, sid: &Sid) -> Result<(), SandboxError> {
        refuse_reparse(dir)?;
        let flags = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
        let dacl = read_dacl(dir)?;
        if has_ace(&dacl, ACCESS_DENIED_ACE_TYPE, sid, DENY_ALL_MASK, flags) {
            return Ok(());
        }
        apply(
            dir,
            &dacl,
            &[explicit(sid, DENY_ALL_MASK, DENY_ACCESS, flags)],
        )
    }

    /// Remove every explicit entry for the SIDs `sids` from `path`
    /// (`kgraf sandbox remove`). A path that no longer exists is skipped; a
    /// reparse point is refused, never followed (decision 0083).
    pub(crate) fn revoke(path: &Path, sids: &[&str]) -> Result<(), SandboxError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(SandboxError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
        refuse_reparse(path)?;
        let sids = sids
            .iter()
            .map(|s| Sid::from_string(s))
            .collect::<Result<Vec<_>, _>>()?;
        let entries: Vec<EXPLICIT_ACCESS_W> = sids
            .iter()
            .map(|sid| explicit(sid, 0, REVOKE_ACCESS, NO_INHERITANCE))
            .collect();
        let dacl = read_dacl(path)?;
        apply(path, &dacl, &entries)
    }

    /// The folders the sandbox user reads beyond its writable roots: the
    /// policy's read roots (the git common directory, package stores), the
    /// dependency trees, the toolchain, and the folders on `PATH` inside the
    /// owner's profile. The profile itself admits only the owner, SYSTEM and
    /// Administrators, so a tool installed there would not start otherwise.
    /// The toolchain follows `toolchain_reads_from` with rustup's own
    /// defaults (`%USERPROFILE%\.cargo`, `%USERPROFILE%\.rustup`), so the cargo
    /// home's `credentials.toml` is never granted: only its `bin`,
    /// `registry` and `git` folders are.
    fn user_reads(
        policy: &Resolved,
        profile: Option<&Path>,
    ) -> (Vec<PathBuf>, Vec<(String, OsString)>) {
        let mut reads: Vec<PathBuf> = Vec::new();
        let mut push = |p: PathBuf| {
            let p = strip_verbatim(&p);
            if !reads.contains(&p) {
                reads.push(p);
            }
        };
        policy.readable.iter().cloned().for_each(&mut push);
        policy.dependency_reads.iter().cloned().for_each(&mut push);
        let mut env = Vec::new();
        let var = |name: &str, default: &str| -> Option<OsString> {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .or_else(|| profile.map(|p| p.join(default).into_os_string()))
        };
        let cargo = var("CARGO_HOME", ".cargo");
        let rustup = var("RUSTUP_HOME", ".rustup");
        // A command that runs as another user would look for the toolchain in
        // that user's profile; it is told where the owner's is, also after
        // an environment clear.
        for (name, value) in [("CARGO_HOME", &cargo), ("RUSTUP_HOME", &rustup)] {
            if let Some(v) = value
                && Path::new(v).is_dir()
            {
                env.push((name.to_owned(), v.clone()));
            }
        }
        let toolchain = super::super::toolchain_reads_from(rustup, cargo, profile);
        toolchain.reads.into_iter().for_each(&mut push);
        if let (Some(profile), Some(path)) = (profile, std::env::var_os("PATH")) {
            for dir in std::env::split_paths(&path) {
                if let Ok(real) = kgraf_core::plain_path::canonicalize(&dir)
                    && real.is_dir()
                    && real.starts_with(profile)
                    && real != profile
                {
                    push(real);
                }
            }
        }
        (reads, env)
    }

    /// Deny writes to `path` for `sid` (the top-level `.git` entry). Deny ACEs
    /// are ordered before allows by `SetEntriesInAclW`.
    fn deny_writes(path: &Path, sid: &Sid) -> Result<(), SandboxError> {
        let flags = if path.is_dir() {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            NO_INHERITANCE
        };
        let dacl = read_dacl(path)?;
        if has_ace(&dacl, ACCESS_DENIED_ACE_TYPE, sid, DENY_WRITE_MASK, flags) {
            return Ok(());
        }
        apply(
            path,
            &dacl,
            &[explicit(sid, DENY_WRITE_MASK, DENY_ACCESS, flags)],
        )
    }

    /// `\\?\C:\x` -> `C:\x`, through the one rule in
    /// `kgraf_core::plain_path`. Policy paths are canonical and so already
    /// plain; this covers a verbatim path given from elsewhere. A verbatim path
    /// whose plain form names a different file (`\\?\C:\a.` is not `C:\a`)
    /// stays verbatim, so an ACL never lands on a different object. The
    /// capability SID is derived from this string, so every root goes through
    /// this one function before its SID is made.
    fn strip_verbatim(path: &Path) -> PathBuf {
        kgraf_core::plain_path::simplify(path).into_owned()
    }

    // ---- reparse, volume and integrity-label guards ----------------------------

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    /// The Low integrity level SID (`S-1-16-4096`).
    const LOW_INTEGRITY_SID: &str = "S-1-16-4096";

    /// Refuse a path that is a reparse point (junction or symlink), so a
    /// planted junction cannot redirect an ACL or label change elsewhere
    /// `symlink_metadata` does not follow the final component.
    fn refuse_reparse(path: &Path) -> Result<(), SandboxError> {
        use std::os::windows::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).map_err(|source| SandboxError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SandboxError::PlatformSetup {
                step: "reject a reparse point before changing its security",
                source: io::Error::other(format!(
                    "{} is a reparse point (junction or symlink); refusing to follow it",
                    path.display()
                )),
            });
        }
        Ok(())
    }

    /// Refuse a writable root on a volume whose filesystem does not keep ACLs
    /// (FAT, exFAT, some SMB shares); the whole mechanism is ACL-based.
    fn require_acl_volume(root: &Path) -> Result<(), SandboxError> {
        // The volume-root prefix, e.g. `C:\`. Without a prefix, let the ACL
        // calls themselves fail rather than guess.
        let Some(std::path::Component::Prefix(prefix)) = root.components().next() else {
            return Ok(());
        };
        let mut mount = prefix.as_os_str().to_os_string();
        mount.push("\\");
        let mount = wide(&mount);
        let mut flags = 0u32;
        // SAFETY: `mount` is NUL-terminated; the name and serial buffers are
        // null (not requested); `flags` receives the filesystem flags.
        let ok = unsafe {
            GetVolumeInformationW(
                mount.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut flags,
                ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(last_error("GetVolumeInformationW"));
        }
        if flags & FILE_PERSISTENT_ACLS == 0 {
            return Err(SandboxError::PlatformSetup {
                step: "require an ACL-preserving filesystem for a writable root",
                source: io::Error::other(format!(
                    "the volume of {} does not preserve ACLs (FAT, exFAT or a share); \
                     the sandbox confines writes with ACLs and cannot use it",
                    root.display()
                )),
            });
        }
        Ok(())
    }

    /// Build the Low integrity SID.
    fn low_sid() -> Result<Sid, SandboxError> {
        Sid::from_string(LOW_INTEGRITY_SID)
    }

    /// Label `root` with a Low mandatory label (`no-write-up`, inherited), so a
    /// Low-integrity sandbox can write there while still being unable to write
    /// up to Medium objects elsewhere.
    fn label_root_low(root: &Path) -> Result<(), SandboxError> {
        let low = low_sid()?;
        // ACL buffer: header + one mandatory-label ACE + the label SID.
        let mut acl = vec![0u32; 64];
        let bytes = std::mem::size_of_val(acl.as_slice()) as u32;
        // SAFETY: `acl` is a DWORD-aligned buffer of `bytes` bytes.
        if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), bytes, ACL_REVISION) } == 0 {
            return Err(last_error("InitializeAcl(label)"));
        }
        // SAFETY: a valid empty ACL and a valid label SID.
        let ok = unsafe {
            AddMandatoryAce(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                low.as_psid(),
            )
        };
        if ok == 0 {
            return Err(last_error("AddMandatoryAce"));
        }
        let name = wide(root.as_os_str());
        // SAFETY: `name` is NUL-terminated; the SACL is a valid ACL.
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                acl.as_ptr().cast(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(root, code));
        }
        Ok(())
    }

    // ---- prepare --------------------------------------------------------------

    /// Capability SIDs for the policy's writable roots, whose ACLs are in place.
    #[derive(Debug)]
    pub(in crate::sandbox) struct Prepared {
        caps: Vec<Sid>,
        /// Identifies the workspace, so its private desktop is reused.
        desktop_key: String,
        /// When set, the command starts as this sandbox user through the
        /// runner (decision 0097) instead of under the restricted token.
        user: Option<SandboxUser>,
    }

    /// The sandbox user a command starts as, and the runner that starts it.
    #[derive(Debug)]
    pub(in crate::sandbox) struct SandboxUser {
        user: &'static LoadedUser,
        runner: &'static Path,
        /// Variables the command gets unless it sets them itself: where the
        /// owner's toolchain is (`user_reads`).
        env: Vec<(String, OsString)>,
        /// The worker slot the command runs in, held until the command's
        /// job is closed, so no other folder takes the user while it runs.
        slot: Arc<Lease>,
    }

    /// The strength of the mode this process uses: the sandbox user once
    /// setup has run, the restricted token otherwise. An error when setup
    /// ran but its users cannot be used (`state`).
    pub(in crate::sandbox) fn strength() -> Result<IsolationStrength, SandboxError> {
        Ok(match state::users()? {
            Some(_) => USER_STRENGTH,
            None => STRENGTH,
        })
    }

    /// Whether this process uses the restricted token, whose isolation is
    /// partial and needs the policy's opt-in.
    pub(in crate::sandbox) fn is_partial() -> Result<bool, SandboxError> {
        Ok(state::users()?.is_none())
    }

    /// Check what the backend can enforce and set the root ACLs. Runs when the
    /// command is built, so a failure surfaces before anything starts.
    pub(in crate::sandbox) fn prepare(policy: &Resolved) -> Result<Prepared, SandboxError> {
        if let Some(users) = state::users()? {
            return prepare_for_user(policy, users);
        }
        if policy.network == Network::Deny {
            return Err(SandboxError::PolicyNotEnforceable {
                backend: BACKEND_NAME,
                what: "network denial (without administrator rights only AppContainer can deny \
                       sockets, and AppContainer breaks the toolchains)",
            });
        }
        if matches!(policy.network, Network::Proxy { .. }) {
            return Err(SandboxError::PolicyNotEnforceable {
                backend: BACKEND_NAME,
                what: "network through the egress proxy only (without administrator rights no \
                       destination can be denied, so a command could pass the proxy by)",
            });
        }
        if !policy.unix_sockets.is_empty() {
            return Err(SandboxError::PolicyNotEnforceable {
                backend: BACKEND_NAME,
                what: "a unix-socket allowlist (a socket outside the writable roots stays \
                       write-denied, so an allowlisted socket would not open)",
            });
        }
        let roots: Vec<PathBuf> = policy.writable.iter().map(|r| strip_verbatim(r)).collect();
        let caps = roots
            .iter()
            .map(|root| Sid::from_string(&capability_sid_string(&root.to_string_lossy())))
            .collect::<Result<Vec<_>, _>>()?;
        for (root, sid) in roots.iter().zip(&caps) {
            // Order matters: refuse a junction and a non-ACL volume before any
            // security call, so neither is followed or silently ineffective.
            refuse_reparse(root)?;
            require_acl_volume(root)?;
            grant_writable_root(root, sid)?;
            label_root_low(root)?;
            verify_effective_write(root, sid)?;
        }
        // Deny writes to a real (non-reparse) `.git` under a writable root. A
        // planted `scratch\.git` junction is refused by `refuse_reparse`, so an
        // ACL change is never redirected onto the user's profile. Every
        // capability SID, since a root nested above another would otherwise
        // reach the inner `.git` through inheritance.
        for root in &roots {
            let gitlink = root.join(".git");
            // `symlink_metadata`, not `exists`: `exists` follows a link, so a
            // dangling `.git` link or junction read as absent and was skipped
            // instead of refused.
            match std::fs::symlink_metadata(&gitlink) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(SandboxError::Io {
                        path: gitlink,
                        source,
                    });
                }
            }
            refuse_reparse(&gitlink)?;
            for sid in &caps {
                deny_writes(&gitlink, sid)?;
            }
        }
        // A hidden folder (`SandboxPolicy::hide`): a deny ACE for the
        // capability of every writable root above it, inherited by what is in
        // it. A writable root beneath it has its own capability, which is not
        // denied, and its own explicit grant, which precedes the inherited
        // deny (`hidden_denials`). Reads are not confined on this backend.
        for (dir, sids) in super::hidden_denials(&policy.hidden, &roots, &caps, strip_verbatim) {
            refuse_reparse(&dir)?;
            for sid in sids {
                deny_writes(&dir, sid)?;
            }
        }
        // The first writable root (the worktree root under the standard policy)
        // names the workspace for desktop reuse.
        let desktop_key = roots
            .first()
            .map(|r| r.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        Ok(Prepared {
            caps,
            desktop_key,
            user: None,
        })
    }

    /// [`prepare`] once setup has run: pick the sandbox user for the policy's
    /// network and grant it write on the writable roots. The offline user
    /// runs a command with the network denied or through the egress proxy
    /// (its firewall rules leave only the proxy's ports open); the online
    /// user runs one with the network open.
    fn prepare_for_user(
        policy: &Resolved,
        users: &'static Users,
    ) -> Result<Prepared, SandboxError> {
        if !policy.unix_sockets.is_empty() {
            return Err(SandboxError::PolicyNotEnforceable {
                backend: USER_BACKEND_NAME,
                what: "a unix-socket allowlist (a socket is confined only by its own ACL)",
            });
        }
        // The slot pool: the users without the network run a command with
        // the network denied or through the egress proxy; the users with it
        // run one with the network open.
        let pool = match policy.network {
            Network::Allow => Pool::Online,
            Network::Deny => Pool::Offline,
            Network::Proxy { port } if PROXY_PORTS.contains(&port) => Pool::Offline,
            Network::Proxy { .. } => {
                return Err(SandboxError::PolicyNotEnforceable {
                    backend: USER_BACKEND_NAME,
                    what: "an egress proxy port outside 47100 to 47131, the only loopback \
                           ports the offline sandbox user's firewall rules leave open",
                });
            }
        };
        let roots: Vec<PathBuf> = policy.writable.iter().map(|r| strip_verbatim(r)).collect();
        // The slot of the command's own folder (its first writable root):
        // the worker's, else the one this process holds for the folder, else
        // one for this command alone. Two folders never share a slot, so a
        // command cannot write the folder of another that runs at the same
        // time; when every slot is held by another folder, it is refused.
        // The slot is recorded with every root before any grant is written,
        // so the grants are revoked when the slot moves to another folder.
        let slot = users.lease(pool, policy.worker_slot.as_ref(), &roots, COMMAND_WAIT)?;
        let user = users.user(slot.role())?;
        let sid = Sid::from_string(&user.sid)?;
        for root in &roots {
            // As in `prepare`: refuse a junction and a non-ACL volume before
            // any security call.
            refuse_reparse(root)?;
            require_acl_volume(root)?;
            grant_writable_root(root, &sid)?;
        }
        for root in &roots {
            let gitlink = root.join(".git");
            match std::fs::symlink_metadata(&gitlink) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(SandboxError::Io {
                        path: gitlink,
                        source,
                    });
                }
            }
            refuse_reparse(&gitlink)?;
            deny_writes(&gitlink, &sid)?;
        }
        // A hidden folder always lies beneath a writable root (the policy
        // checks it), so the sandbox user is denied there.
        for hidden in &policy.hidden {
            deny_all(&strip_verbatim(&hidden.dir), &sid)?;
        }
        let profile = std::env::var_os("USERPROFILE")
            .filter(|p| !p.is_empty())
            .and_then(|p| kgraf_core::plain_path::canonicalize(PathBuf::from(p)).ok());
        let (reads, env) = user_reads(policy, profile.as_deref());
        for dir in &reads {
            // A read root inside a writable root is already granted there.
            if roots.iter().any(|r| dir.starts_with(r)) {
                continue;
            }
            if grant_read(dir, &sid)? {
                users.record_grant(dir)?;
            }
        }
        // The runner must be readable and runnable by every sandbox user,
        // wherever kGraf is installed.
        for u in users.all() {
            if grant_read(&users.runner, &Sid::from_string(&u.sid)?)? {
                users.record_grant(&users.runner)?;
            }
        }
        Ok(Prepared {
            caps: Vec::new(),
            desktop_key: String::new(),
            user: Some(SandboxUser {
                user,
                runner: &users.runner,
                env,
                slot,
            }),
        })
    }

    /// [`crate::worker_slot`]: the slot without the network for the folder
    /// `root`, or `None` before `kgraf sandbox setup`.
    pub(in crate::sandbox) fn worker_slot(
        root: &Path,
    ) -> Result<Option<crate::sandbox::WorkerSlot>, SandboxError> {
        let Some(users) = state::users()? else {
            return Ok(None);
        };
        let root = strip_verbatim(&super::super::resolve(root)?);
        let slot = users.lease(
            Pool::Offline,
            None,
            std::slice::from_ref(&root),
            std::time::Duration::ZERO,
        )?;
        Ok(Some(crate::sandbox::WorkerSlot(slot)))
    }

    /// Confirm the restricted token is actually granted write on `root` and on
    /// a child, with `AccessCheck`, not just that the ACE is present: a partial
    /// propagation must fail loud.
    /// The write ACL and the Low label [`prepare`] gives one writable root,
    /// set before kGraf makes a dependency junction in it
    /// (`crate::dependencies`). Writing a folder's ACL propagates its
    /// inheritable entries to what is in it, and the write grant is written
    /// only while it is absent ([`grant_writable_root`]), so a junction made
    /// afterwards never sees that propagation. Unverified on Windows: whether
    /// the propagation would follow a junction at all; the Low label is
    /// written again on each command (see `gaps`).
    pub(in crate::sandbox) fn prepare_root(root: &Path) -> Result<(), SandboxError> {
        let root = strip_verbatim(root);
        if let Some(users) = state::users()? {
            // This runs before kGraf makes a dependency junction in the root.
            // Only the user of the root's own slot gets the grant: the
            // worker's slot when one holds this folder, else the slot the
            // folder is bound to, which its commands take again. Granting
            // every sandbox user here let the user of any other folder write
            // this one. ([`prepare_for_user`] writes the grant again, a no-op
            // while it is present.)
            refuse_reparse(&root)?;
            require_acl_volume(&root)?;
            let slot = users.lease(
                Pool::Offline,
                None,
                std::slice::from_ref(&root),
                COMMAND_WAIT,
            )?;
            let user = users.user(slot.role())?;
            grant_writable_root(&root, &Sid::from_string(&user.sid)?)?;
            return Ok(());
        }
        let sid = Sid::from_string(&capability_sid_string(&root.to_string_lossy()))?;
        refuse_reparse(&root)?;
        require_acl_volume(&root)?;
        grant_writable_root(&root, &sid)?;
        label_root_low(&root)?;
        verify_effective_write(&root, &sid)
    }

    fn verify_effective_write(root: &Path, cap: &Sid) -> Result<(), SandboxError> {
        let token = restricted_token_for(std::slice::from_ref(cap))?.0;
        // An impersonation token, which is what AccessCheck requires.
        let mut imp: HANDLE = ptr::null_mut();
        // SAFETY: `token` is a valid primary token; output written on success.
        let ok = unsafe {
            DuplicateTokenEx(
                token.as_raw_handle(),
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut imp,
            )
        };
        if ok == 0 {
            return Err(last_error("DuplicateTokenEx(AccessCheck)"));
        }
        let imp = owned(imp, "DuplicateTokenEx")?;
        let probe = |path: &Path| effective_write_granted(path, &imp);
        if !probe(root)? {
            return Err(access_gap(root));
        }
        // A child directory created for the probe (removed after). If the tree
        // ACE did not propagate, this fails. A fresh name per probe, created
        // with `create_dir` (never `create_dir_all`): two commands prepared
        // on one root at once no longer remove each other's probe, and an
        // entry already at that name (a planted junction) fails loud instead
        // of being probed in its target's place.
        sweep_stale_probes(root);
        static PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let child = root.join(format!(
            "{}{}-{n}",
            super::ACL_PROBE_PREFIX,
            std::process::id()
        ));
        std::fs::create_dir(&child).map_err(|source| SandboxError::Io {
            path: child.clone(),
            source,
        })?;
        let granted = probe(&child);
        let _ = std::fs::remove_dir_all(&child);
        if !granted? {
            return Err(access_gap(&child));
        }
        Ok(())
    }

    /// Remove the probe directories a process that died mid-check left in
    /// `root`, where they would stay as untracked entries of the worktree.
    /// Only a probe named for a process that is no longer running is removed
    /// (a live one, this process included, may be probing now), only a plain
    /// directory (never a junction or link planted under that name), and only
    /// an empty one (`remove_dir`). Best effort: a probe that cannot be
    /// removed is left, and the check itself goes on.
    fn sweep_stale_probes(root: &Path) {
        use std::os::windows::fs::MetadataExt;
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(super::acl_probe_pid) else {
                continue;
            };
            if pid == std::process::id() || process_running(pid) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                let _ = std::fs::remove_dir(&path);
            }
        }
    }

    /// Whether a process with id `pid` is running now. A process that exists
    /// but cannot be opened counts as running, so its probe is left alone.
    fn process_running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // STILL_ACTIVE, the exit code of a process that has not ended.
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: no pointers; a null result is checked before use.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            // SAFETY: reads this thread's last error, set by the call above.
            return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
        }
        let Ok(handle) = owned(raw, "OpenProcess") else {
            return true;
        };
        let mut code = 0u32;
        // SAFETY: a valid process handle and a valid output pointer.
        let ok = unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) };
        ok == 0 || code == STILL_ACTIVE
    }

    fn access_gap(path: &Path) -> SandboxError {
        SandboxError::PlatformSetup {
            step: "verify effective write access under the restricted token",
            source: io::Error::other(format!(
                "the restricted token is not granted write on {}; the ACL did not fully \
                 propagate",
                path.display()
            )),
        }
    }

    /// Whether `token` (impersonation) is granted `FILE_GENERIC_WRITE` on `path`.
    fn effective_write_granted(path: &Path, token: &OwnedHandle) -> Result<bool, SandboxError> {
        let name = wide(path.as_os_str());
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; the descriptor is freed by `Local`.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sd,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        let _guard = Local(sd);
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        let mut privset = PRIVILEGE_SET::default();
        let mut privlen = std::mem::size_of::<PRIVILEGE_SET>() as u32;
        let mut granted = 0u32;
        let mut status = 0;
        // SAFETY: a valid self-relative descriptor and impersonation token;
        // all output pointers are valid and sized.
        let ok = unsafe {
            AccessCheck(
                sd,
                token.as_raw_handle(),
                FILE_GENERIC_WRITE,
                &mapping,
                &mut privset,
                &mut privlen,
                &mut granted,
                &mut status,
            )
        };
        if ok == 0 {
            return Err(last_error("AccessCheck"));
        }
        Ok(status != 0)
    }

    // ---- token ----------------------------------------------------------------

    fn logon_sid(token: &OwnedHandle) -> Result<Sid, SandboxError> {
        let h = token.as_raw_handle();
        let mut needed = 0u32;
        // SAFETY: size query with a null buffer.
        unsafe { GetTokenInformation(h, TokenGroups, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenGroups) size"));
        }
        // u64 backing keeps TOKEN_GROUPS and its pointers aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` holds `needed` bytes.
        if unsafe {
            GetTokenInformation(h, TokenGroups, buf.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(last_error("GetTokenInformation(TokenGroups)"));
        }
        let groups = buf.as_ptr() as *const TOKEN_GROUPS;
        // SAFETY: the system wrote a TOKEN_GROUPS whose `Groups` array has
        // `GroupCount` entries inside `buf`.
        let entries = unsafe {
            std::slice::from_raw_parts((*groups).Groups.as_ptr(), (*groups).GroupCount as usize)
        };
        let logon = entries
            .iter()
            .find(|g| g.Attributes & (SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32)
            .ok_or_else(|| SandboxError::PlatformSetup {
                step: "find the logon SID",
                source: io::Error::other("the process token has no logon SID"),
            })?;
        // SAFETY: the SID pointer refers into `buf`, still alive.
        unsafe { Sid::copy_from(logon.Sid) }
    }

    /// The token's user SID (`TokenUser`).
    fn user_sid(token: &OwnedHandle) -> Result<Sid, SandboxError> {
        let h = token.as_raw_handle();
        let mut needed = 0u32;
        // SAFETY: size query with a null buffer.
        unsafe { GetTokenInformation(h, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenUser) size"));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` holds `needed` bytes.
        if unsafe {
            GetTokenInformation(h, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(last_error("GetTokenInformation(TokenUser)"));
        }
        let user = buf.as_ptr() as *const TOKEN_USER;
        // SAFETY: the system wrote a TOKEN_USER whose SID points into `buf`.
        unsafe { Sid::copy_from((*user).User.Sid) }
    }

    /// A fresh, unique SID for one spawn. Granted in the default DACL and added
    /// to the restricting set, it isolates one spawn's process tree from every
    /// other sandbox's: only this spawn's processes pass the restricting
    /// write check on each other. Shape follows `capability_sid_string`. A
    /// private desktop's SID is made the same way ([`Desktop::create`]).
    fn per_spawn_sid() -> Result<Sid, SandboxError> {
        let (a, b) = (random_u64(), random_u64());
        Sid::from_string(&format!(
            "S-1-5-21-{}-{}-{}-{}",
            (a >> 32) as u32,
            a as u32,
            (b >> 32) as u32,
            b as u32
        ))
    }

    /// Objects the sandboxed process creates without an inheritable ACL (its
    /// own children, pipes, events) are reachable from this spawn's own tree
    /// only. The default DACL grants the user SID (for the process's own normal
    /// access check) and the per-spawn SID (a restricting SID unique to this
    /// spawn), never the logon SID (which every same-session sandbox shares and
    /// which allowed cross-sandbox process injection). OWNER RIGHTS removes the
    /// owner's implicit `WRITE_DAC` (codex).
    fn set_default_dacl(token: &OwnedHandle, user: &Sid, spawn: &Sid) -> Result<(), SandboxError> {
        let owner_rights = Sid::from_string("S-1-3-4")?;
        let entries = [
            explicit(user, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
            explicit(spawn, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
            explicit(&owner_rights, READ_CONTROL, GRANT_ACCESS, NO_INHERITANCE),
        ];
        let mut acl: *mut ACL = ptr::null_mut();
        // SAFETY: valid entries, no old ACL; result freed by `Local`.
        let code = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                ptr::null(),
                &mut acl,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(win32("SetEntriesInAclW(default DACL)", code));
        }
        let _guard = Local(acl.cast());
        let info = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
        // SAFETY: `info` is a TOKEN_DEFAULT_DACL of the stated size.
        let ok = unsafe {
            SetTokenInformation(
                token.as_raw_handle(),
                TokenDefaultDacl,
                ptr::addr_of!(info).cast(),
                std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("SetTokenInformation(TokenDefaultDacl)"));
        }
        Ok(())
    }

    /// Bypass-traverse checking. Without it, opening a path fails wherever an
    /// ancestor directory denies listing (codex enables it the same way).
    fn enable_change_notify(token: &OwnedHandle) -> Result<(), SandboxError> {
        let name = wide(OsStr::new("SeChangeNotifyPrivilege"));
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        // SAFETY: `name` is NUL-terminated.
        if unsafe { LookupPrivilegeValueW(ptr::null(), name.as_ptr(), &mut luid) } == 0 {
            return Err(last_error("LookupPrivilegeValueW"));
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // SAFETY: a valid TOKEN_PRIVILEGES; previous state not requested.
        let ok = unsafe {
            AdjustTokenPrivileges(
                token.as_raw_handle(),
                0,
                &privileges,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        // AdjustTokenPrivileges reports a missing privilege through
        // GetLastError even when it returns success.
        // SAFETY: plain call.
        if ok == 0 || unsafe { GetLastError() } != ERROR_SUCCESS {
            return Err(last_error("AdjustTokenPrivileges(SeChangeNotifyPrivilege)"));
        }
        Ok(())
    }

    /// Drop the token's integrity level to Low. A Low process cannot write up
    /// to a Medium object (the user's files, an Everyone-writable Medium
    /// directory) and UIPI blocks `SendInput` to the user's desktop, whatever
    /// the `WRITE_DAC` semantics of the restricted token are.
    fn set_integrity_low(token: &OwnedHandle) -> Result<(), SandboxError> {
        let low = low_sid()?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: low.as_psid(),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        // SAFETY: `label` is a valid TOKEN_MANDATORY_LABEL; the size includes
        // the SID it points at.
        let ok = unsafe {
            SetTokenInformation(
                token.as_raw_handle(),
                TokenIntegrityLevel,
                ptr::addr_of!(label).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(low.as_psid()),
            )
        };
        if ok == 0 {
            return Err(last_error("SetTokenInformation(TokenIntegrityLevel=Low)"));
        }
        Ok(())
    }

    /// A restricted primary token whose restricting SIDs are exactly
    /// `restricting`, with change-notify enabled but no default DACL or
    /// integrity change. Returns the token, its user SID and its logon SID.
    fn restricted_token_for(restricting: &[Sid]) -> Result<(OwnedHandle, Sid, Sid), SandboxError> {
        let mut raw: HANDLE = ptr::null_mut();
        // SAFETY: pseudo-handle of this process; output written on success.
        let ok = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE
                    | TOKEN_QUERY
                    | TOKEN_ASSIGN_PRIMARY
                    | TOKEN_ADJUST_DEFAULT
                    | TOKEN_ADJUST_PRIVILEGES,
                &mut raw,
            )
        };
        if ok == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        let base = owned(raw, "OpenProcessToken")?;
        let user = user_sid(&base)?;
        let logon = logon_sid(&base)?;
        let restricting_sa: Vec<SID_AND_ATTRIBUTES> = restricting
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: 0,
            })
            .collect();
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `restricting_sa` points at SIDs that outlive the call.
        let ok = unsafe {
            CreateRestrictedToken(
                base.as_raw_handle(),
                DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
                0,
                ptr::null(),
                0,
                ptr::null(),
                restricting_sa.len() as u32,
                restricting_sa.as_ptr(),
                &mut token,
            )
        };
        if ok == 0 {
            return Err(last_error("CreateRestrictedToken"));
        }
        let token = owned(token, "CreateRestrictedToken")?;
        enable_change_notify(&token)?;
        Ok((token, user, logon))
    }

    /// The primary token for a spawn: restricted by the capability SIDs, a
    /// fresh per-spawn SID, the SID of the private desktop it runs on (see
    /// [`Desktop`]) and Everyone; default DACL scoped to the user and the
    /// per-spawn SID; integrity dropped to Low.
    fn build_sandbox_token(
        caps: &[Sid],
        desktop: Option<&Sid>,
    ) -> Result<OwnedHandle, SandboxError> {
        let per_spawn = per_spawn_sid()?;
        let everyone = Sid::from_string("S-1-1-0")?;
        let mut restricting: Vec<Sid> = Vec::with_capacity(caps.len() + 3);
        restricting.extend(caps.iter().cloned());
        restricting.push(per_spawn.clone());
        restricting.extend(desktop.cloned());
        restricting.push(everyone);
        let (token, user, _logon) = restricted_token_for(&restricting)?;
        set_default_dacl(&token, &user, &per_spawn)?;
        set_integrity_low(&token)?;
        Ok(token)
    }

    /// The user SID of this process's token: the SID a sandboxed process
    /// passes the normal access check with.
    fn process_user_sid() -> Result<Sid, SandboxError> {
        let mut raw: HANDLE = ptr::null_mut();
        // SAFETY: pseudo-handle of this process; output written on success.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(last_error("OpenProcessToken(TOKEN_QUERY)"));
        }
        user_sid(&owned(raw, "OpenProcessToken(TOKEN_QUERY)")?)
    }

    // ---- job and desktop --------------------------------------------------------

    /// A job that ends its processes when its last handle closes, allows no
    /// breakaway, and applies the UI limits `ui` (`JOB_OBJECT_UILIMIT_*`).
    /// With `ui` 0 no UI limit class is set at all, so the job can nest.
    fn create_job(ui: u32) -> Result<OwnedHandle, SandboxError> {
        // SAFETY: anonymous job, default security.
        let job = owned(
            unsafe { CreateJobObjectW(ptr::null(), ptr::null()) },
            "CreateJobObjectW",
        )?;
        // SAFETY: zeroed is a valid initial value for this plain C struct.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // No JOB_OBJECT_LIMIT_BREAKAWAY_OK: CREATE_BREAKAWAY_FROM_JOB fails.
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        // SAFETY: a valid struct of the stated size and class.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::addr_of!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(extended limits)"));
        }
        if ui == 0 {
            return Ok(job);
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: ui,
        };
        // SAFETY: as above.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicUIRestrictions,
                ptr::addr_of!(ui).cast(),
                std::mem::size_of_val(&ui) as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(UI restrictions)"));
        }
        Ok(job)
    }

    /// Rights the sandbox gets on its desktop: everything except switching to
    /// it, deleting it and changing its DACL or owner.
    const DESKTOP_PARTICIPANT: u32 = DESKTOP_READOBJECTS
        | DESKTOP_CREATEWINDOW
        | DESKTOP_CREATEMENU
        | DESKTOP_HOOKCONTROL
        | DESKTOP_JOURNALRECORD
        | DESKTOP_JOURNALPLAYBACK
        | DESKTOP_ENUMERATE
        | DESKTOP_WRITEOBJECTS
        | DESKTOP_READ_CONTROL;

    struct Desktop {
        handle: HDESK,
        /// `<WindowStation>\<name>`, NUL-terminated, for
        /// `STARTUPINFOW::lpDesktop`.
        startup_name: Vec<u16>,
        /// A fresh SID granted on this desktop and on nothing else. Every
        /// command that runs here carries it as a restricting SID, so the
        /// desktop's write rights pass the restricting check; without it they
        /// fail and `user32.dll` cannot load (see the module header).
        sid: Sid,
    }

    // SAFETY: a desktop handle is a kernel handle usable from any thread.
    unsafe impl Send for Desktop {}
    // SAFETY: no interior state beyond the handle, an immutable name and an
    // immutable SID.
    unsafe impl Sync for Desktop {}

    /// The current process's window station name (`WinSta0` interactively, but
    /// a distinct name under a service or a scheduled, non-interactive run;
    /// decision 0052). Never hard-coded.
    fn window_station_name() -> Result<String, SandboxError> {
        // SAFETY: the returned handle is a pseudo-handle not to be closed.
        let winsta = unsafe { GetProcessWindowStation() };
        if winsta.is_null() {
            return Err(last_error("GetProcessWindowStation"));
        }
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        // SAFETY: `buf` has the stated size in bytes.
        let ok = unsafe {
            GetUserObjectInformationW(
                winsta,
                UOI_NAME,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(&buf) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error("GetUserObjectInformationW(window station)"));
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len]))
    }

    impl Desktop {
        fn create(user: &Sid) -> Result<Self, SandboxError> {
            let name = format!("kGrafSandbox-{:016x}", random_u64());
            let wname = wide(OsStr::new(&name));
            // Everything that can fail without a handle comes first, so no
            // early return leaves the desktop handle open.
            let station = window_station_name()?;
            let sid = per_spawn_sid()?;
            // SAFETY: NUL-terminated name, no device or mode, default security;
            // the handle is closed by Drop. Full rights for this process.
            let handle = unsafe {
                CreateDesktopW(
                    wname.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    0x000F_01FF,
                    ptr::null(),
                )
            };
            if handle.is_null() {
                return Err(last_error("CreateDesktopW"));
            }
            let desktop = Self {
                handle,
                startup_name: wide(OsStr::new(&format!("{station}\\{name}"))),
                sid,
            };
            // Replace the DACL: the sandbox participates; nobody else is
            // admitted. Two entries, because a write-restricted token is
            // checked twice: the user SID passes the normal check, and the
            // desktop's own SID, a restricting SID of every command here,
            // passes the restricting check for the desktop's write rights.
            // Another sandbox runs on its own desktop with another SID.
            let entries = [
                explicit(user, DESKTOP_PARTICIPANT, GRANT_ACCESS, NO_INHERITANCE),
                explicit(
                    &desktop.sid,
                    DESKTOP_PARTICIPANT,
                    GRANT_ACCESS,
                    NO_INHERITANCE,
                ),
            ];
            let mut acl: *mut ACL = ptr::null_mut();
            // SAFETY: valid entries, no old ACL.
            let code = unsafe {
                SetEntriesInAclW(
                    entries.len() as u32,
                    entries.as_ptr(),
                    ptr::null(),
                    &mut acl,
                )
            };
            if code != ERROR_SUCCESS {
                return Err(win32("SetEntriesInAclW(desktop)", code));
            }
            let _guard = Local(acl.cast());
            // SAFETY: a desktop handle with WRITE_DAC and a valid ACL.
            let code = unsafe {
                SetSecurityInfo(
                    desktop.handle,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl,
                    ptr::null(),
                )
            };
            if code != ERROR_SUCCESS {
                return Err(win32("SetSecurityInfo(desktop)", code));
            }
            // Low-label the desktop so a Low-integrity sandbox can use it while
            // still being blocked (UIPI) from the user's Medium desktop.
            desktop.label_low()?;
            Ok(desktop)
        }

        fn label_low(&self) -> Result<(), SandboxError> {
            let low = low_sid()?;
            let mut acl = vec![0u32; 64];
            let bytes = std::mem::size_of_val(acl.as_slice()) as u32;
            // SAFETY: a DWORD-aligned buffer of `bytes` bytes.
            if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), bytes, ACL_REVISION) } == 0 {
                return Err(last_error("InitializeAcl(desktop label)"));
            }
            // SAFETY: a valid empty ACL and a valid label SID.
            if unsafe {
                AddMandatoryAce(
                    acl.as_mut_ptr().cast(),
                    ACL_REVISION,
                    NO_INHERITANCE,
                    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                    low.as_psid(),
                )
            } == 0
            {
                return Err(last_error("AddMandatoryAce(desktop)"));
            }
            // SAFETY: a desktop handle with WRITE_OWNER and a valid SACL.
            let code = unsafe {
                SetSecurityInfo(
                    self.handle,
                    SE_WINDOW_OBJECT,
                    LABEL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                    acl.as_ptr().cast(),
                )
            };
            if code != ERROR_SUCCESS {
                return Err(win32("SetSecurityInfo(desktop label)", code));
            }
            Ok(())
        }
    }

    impl Drop for Desktop {
        fn drop(&mut self) {
            // SAFETY: a desktop handle this struct owns. The desktop itself
            // lives on while a process still uses it.
            unsafe { CloseDesktop(self.handle) };
        }
    }

    /// One private desktop per workspace, reused across spawns. Held for
    /// the process lifetime; a worktree is short-lived and few run at once.
    fn desktop_for(key: &str) -> Result<std::sync::Arc<Desktop>, SandboxError> {
        use std::sync::{Arc, Mutex, OnceLock};
        static DESKTOPS: OnceLock<Mutex<std::collections::HashMap<String, Arc<Desktop>>>> =
            OnceLock::new();
        let map = DESKTOPS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let mut map = map.lock().map_err(|_| SandboxError::PlatformSetup {
            step: "lock the private-desktop cache",
            source: io::Error::other("the desktop cache mutex was poisoned"),
        })?;
        if let Some(desktop) = map.get(key) {
            return Ok(Arc::clone(desktop));
        }
        let desktop = Arc::new(Desktop::create(&process_user_sid()?)?);
        map.insert(key.to_string(), Arc::clone(&desktop));
        Ok(desktop)
    }

    /// A name suffix that is unique enough to avoid collisions between
    /// concurrent sandboxes. Not a security value: the DACL is the boundary.
    fn random_u64() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        h.finish()
    }

    // ---- stdio ----------------------------------------------------------------

    /// One standard stream: the handle the child inherits and, when piped,
    /// the parent's end.
    struct Stream {
        child: Option<OwnedHandle>,
        parent: Option<OwnedHandle>,
    }

    fn inheritable(h: HANDLE, step: &'static str) -> Result<OwnedHandle, SandboxError> {
        let mut dup: HANDLE = ptr::null_mut();
        // SAFETY: duplicates a valid handle within this process.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h,
                GetCurrentProcess(),
                &mut dup,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(last_error(step));
        }
        owned(dup, step)
    }

    fn stream(which: STD_HANDLE, redirect: Redirect) -> Result<Stream, SandboxError> {
        match redirect {
            Redirect::Null => {
                let sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: ptr::null_mut(),
                    bInheritHandle: 1,
                };
                let nul = wide(OsStr::new("NUL"));
                // SAFETY: NUL-terminated device name, inheritable handle.
                let h = unsafe {
                    CreateFileW(
                        nul.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        &sa,
                        OPEN_EXISTING,
                        0,
                        ptr::null_mut(),
                    )
                };
                Ok(Stream {
                    child: Some(owned(h, "open NUL")?),
                    parent: None,
                })
            }
            Redirect::Piped => {
                let (parent, child) = child_pipe(which != STD_INPUT_HANDLE)?;
                Ok(Stream {
                    child: Some(inheritable(child.as_raw_handle(), "DuplicateHandle(pipe)")?),
                    parent: Some(parent),
                })
            }
            Redirect::Inherit => {
                // SAFETY: plain call; the handle is borrowed, not owned.
                let h = unsafe { GetStdHandle(which) };
                if h.is_null() || h == INVALID_HANDLE_VALUE {
                    // No such stream in this process (a GUI app): the child
                    // gets none either, as std does.
                    return Ok(Stream {
                        child: None,
                        parent: None,
                    });
                }
                let mut mode = 0;
                // SAFETY: probes whether `h` is a console handle.
                if which == STD_INPUT_HANDLE && unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                    return Err(SandboxError::PolicyNotEnforceable {
                        backend: BACKEND_NAME,
                        what: "an inherited console input handle (WriteConsoleInput through it \
                               injects keystrokes into the user's console); use Redirect::Null \
                               or Redirect::Piped",
                    });
                }
                Ok(Stream {
                    child: Some(inheritable(h, "DuplicateHandle(std handle)")?),
                    parent: None,
                })
            }
        }
    }

    /// How many pipe names [`child_pipe`] tries before it gives up: a name
    /// is refused when another pipe already has it.
    const PIPE_NAME_TRIES: u32 = 10;

    /// Bytes each direction of a [`child_pipe`] buffers, as std's own child
    /// pipes do.
    const PIPE_BUFFER: u32 = 64 * 1024;

    /// A pipe to a child: this process's end opened for overlapped I/O, the
    /// child's end for ordinary (synchronous) I/O. Returns (this process's
    /// end, the child's end). With `parent_reads` the child writes (its
    /// stdout or stderr); without it the child reads (its stdin).
    ///
    /// Not `CreatePipe`: both of its ends are synchronous, and std's
    /// `ChildStdin`, `ChildStdout` and `ChildStderr`, which
    /// [`SandboxedChild`] hands out, read and write with `ReadFileEx` and
    /// `WriteFileEx` and then wait in an alertable `SleepEx` for the
    /// completion routine. Their `From<OwnedHandle>` documentation says the
    /// handle must be asynchronous, and std's maintainers state that its
    /// pipe `Read` and `Write` do not work with a synchronous handle
    /// (rust-lang/rust pull request 98704). With a `CreatePipe` handle a
    /// `read` or a `write` can wait for a completion that never comes: a
    /// reader returns nothing or never ends, and a write to stdin never
    /// returns. Only `read_to_end` on the pipe itself worked, because std
    /// does it with a plain `NtReadFile`.
    ///
    /// This is std's own construction (an overlapped end for this process,
    /// a synchronous end for the child), made with the documented Win32
    /// calls its earlier versions used: a named pipe of one instance whose
    /// name holds this process's id and a random part, created with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` so a pipe that already has the name
    /// is refused rather than joined, rejecting remote clients, and opened
    /// at once by this process. Once this process has opened it, a second
    /// open fails with `ERROR_PIPE_BUSY`, so no other process can attach;
    /// if another opened it first, this open fails and the spawn fails
    /// loud.
    fn child_pipe(parent_reads: bool) -> Result<(OwnedHandle, OwnedHandle), SandboxError> {
        let access = if parent_reads {
            PIPE_ACCESS_INBOUND
        } else {
            PIPE_ACCESS_OUTBOUND
        };
        let mut tries = 0;
        let (server, name) = loop {
            tries += 1;
            let name = wide(OsStr::new(&format!(
                r"\\.\pipe\kgraf-sandbox-{}-{:016x}",
                std::process::id(),
                random_u64()
            )));
            // SAFETY: a NUL-terminated name; no security attributes, so the
            // default DACL applies and the handle is not inheritable.
            let h = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    access | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    PIPE_BUFFER,
                    PIPE_BUFFER,
                    0,
                    ptr::null(),
                )
            };
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                break (owned(h, "CreateNamedPipeW")?, name);
            }
            // SAFETY: plain call, made right after the failing one.
            let code = unsafe { GetLastError() };
            // A name another pipe already has is refused with access denied.
            if code != ERROR_ACCESS_DENIED || tries >= PIPE_NAME_TRIES {
                return Err(win32("CreateNamedPipeW", code));
            }
        };
        let client_access = if parent_reads {
            GENERIC_WRITE | FILE_READ_ATTRIBUTES
        } else {
            GENERIC_READ
        };
        // SAFETY: the NUL-terminated name of the pipe made above; no sharing,
        // no security attributes, synchronous I/O (no flags).
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                client_access,
                0,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            // SAFETY: plain call, made right after the failing one.
            let code = unsafe { GetLastError() };
            let step = if code == ERROR_PIPE_BUSY {
                "open the child's end of its pipe (another process opened it first)"
            } else {
                "open the child's end of its pipe"
            };
            return Err(win32(step, code));
        }
        Ok((server, owned(h, "CreateFileW(pipe)")?))
    }

    // ---- program and command line --------------------------------------------

    /// The child's environment block and its `PATH`. `extra` is set last,
    /// over everything else (a POSIX shell's [`SHELL_COMMAND_VAR`]). With
    /// `inherit_all` false (the command's environment was cleared) only
    /// [`super::super::WINDOWS_BASE_VARS`] come from this process; `PATH` is one of
    /// them, so the lookup below stays right.
    fn final_env(
        cmd: &Command,
        extra: Option<(&str, &str)>,
        defaults: &[(String, OsString)],
        inherit_all: bool,
    ) -> (Vec<u16>, Option<OsString>) {
        let inherited: Vec<_> = inherited_vars(std::env::vars_os(), inherit_all)
            .into_iter()
            .map(|(k, v)| (k.encode_wide().collect(), v.encode_wide().collect()))
            .collect();
        let mut overrides: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.encode_wide().collect(),
                    v.map(|v| v.encode_wide().collect()),
                )
            })
            .collect();
        if let Some((name, value)) = extra {
            overrides.push((
                name.encode_utf16().collect(),
                Some(value.encode_utf16().collect()),
            ));
        }
        // A default applies only where the command did not set or remove the
        // variable itself.
        for (name, value) in defaults {
            if !cmd
                .get_envs()
                .any(|(k, _)| k.eq_ignore_ascii_case(OsStr::new(name)))
            {
                overrides.push((
                    name.encode_utf16().collect(),
                    Some(value.encode_wide().collect()),
                ));
            }
        }
        let block = env_block(inherited, overrides);
        // PATH as the child will see it, for program lookup.
        let path = cmd
            .get_envs()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.map(OsStr::to_os_string))
            .unwrap_or_else(|| std::env::var_os("PATH"));
        (block, path)
    }

    fn with_exe(candidate: PathBuf) -> Option<PathBuf> {
        if candidate.is_file() {
            return Some(candidate);
        }
        if candidate.extension().is_none() {
            let exe = candidate.with_extension("exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
        None
    }

    /// Resolve the program against the child's PATH (not the parent's, and
    /// never the current directory), so `lpApplicationName` is exact and a
    /// planted binary in the working directory is not picked up.
    fn resolve_program(
        program: &OsStr,
        cwd: &Path,
        path: Option<&OsStr>,
    ) -> Result<PathBuf, SandboxError> {
        let p = Path::new(program);
        let found = if p.is_absolute() {
            with_exe(p.to_path_buf())
        } else if p.components().count() > 1 {
            with_exe(cwd.join(p))
        } else {
            path.into_iter()
                .flat_map(std::env::split_paths)
                .find_map(|dir| with_exe(dir.join(p)))
        };
        found.ok_or_else(|| {
            SandboxError::Spawn(io::Error::new(
                io::ErrorKind::NotFound,
                format!("program {} not found", p.display()),
            ))
        })
    }

    /// The Git Bash launch of the POSIX shell `shell` with `cmd`'s
    /// arguments; a spawn error, saying how to get Git Bash, when none is
    /// found (fail loud: no other shell is tried).
    fn git_bash_launch(shell: &str, cmd: &Command) -> Result<ShellLaunch, SandboxError> {
        let not_run = |kind, why: String| SandboxError::Spawn(io::Error::new(kind, why));
        use crate::git_for_windows::{Probe as _, SystemProbe};
        let bash = SystemProbe
            .find()
            .map_err(|why| not_run(io::ErrorKind::NotFound, why))?;
        let args: Vec<&OsStr> = cmd.get_args().collect();
        shell_launch(shell, &args, bash).map_err(|why| not_run(io::ErrorKind::InvalidInput, why))
    }

    // ---- spawn ----------------------------------------------------------------

    /// A running sandboxed process. Dropping it closes the job, which kills
    /// the process and every descendant still running (the teardown
    /// guarantee); std's `Child` would leave them running.
    #[derive(Debug)]
    pub struct SandboxedChild {
        pub stdin: Option<ChildStdin>,
        pub stdout: Option<ChildStdout>,
        pub stderr: Option<ChildStderr>,
        process: OwnedHandle,
        pid: u32,
        job: OwnedHandle,
        /// Keeps the (shared, per-workspace) desktop alive while the child
        /// runs. `None` for a command that runs as the sandbox user: its
        /// runner holds the command's own window station.
        _desktop: Option<DesktopGuard>,
        /// The worker slot of a command that runs as a sandbox user. Last, so
        /// it is released after the job handle is closed, which ends the
        /// command.
        _slot: Option<Arc<Lease>>,
    }

    /// Debug-printable wrapper so `SandboxedChild` can derive Debug.
    struct DesktopGuard(#[allow(dead_code)] std::sync::Arc<Desktop>);

    impl std::fmt::Debug for DesktopGuard {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Desktop")
        }
    }

    impl SandboxedChild {
        pub fn id(&self) -> u32 {
            self.pid
        }

        /// The process handle, for waiting from an async runtime.
        pub fn process_handle(&self) -> BorrowedHandle<'_> {
            // SAFETY: the handle is owned by `self` and outlives the borrow.
            unsafe { BorrowedHandle::borrow_raw(self.process.as_raw_handle()) }
        }

        fn exit_status(&self) -> io::Result<ExitStatus> {
            let mut code = 0u32;
            // SAFETY: a valid process handle with query access.
            if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(ExitStatus::from_raw(code))
        }

        /// Wait for the process to exit. Closes stdin first, as std does.
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            drop(self.stdin.take());
            // SAFETY: a valid process handle.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } {
                WAIT_OBJECT_0 => self.exit_status(),
                _ => Err(io::Error::last_os_error()),
            }
        }

        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            // SAFETY: a valid process handle.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => self.exit_status().map(Some),
                WAIT_TIMEOUT => Ok(None),
                _ => Err(io::Error::last_os_error()),
            }
        }

        /// Terminate the process and all its descendants.
        pub fn kill(&mut self) -> io::Result<()> {
            // SAFETY: a valid job handle with terminate access.
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// End every process of the command, then wait until the job holds
        /// none, at most `bound`. True when none is left. A process's
        /// handles, its ends of the output pipes among them, close only once
        /// it has ended, and `TerminateJobObject` returns before that: a
        /// reader that waits for the end of a pipe starts its wait after
        /// this, not after the terminate call.
        pub fn kill_tree_and_wait(&mut self, bound: std::time::Duration) -> io::Result<bool> {
            self.kill()?;
            let deadline = std::time::Instant::now() + bound;
            loop {
                if self.active_processes()? == 0 {
                    return Ok(true);
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        /// How many processes the job still holds.
        pub fn active_processes(&self) -> io::Result<u32> {
            // SAFETY: zeroed is a valid value for this plain C struct.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid job handle with query access, and a struct of
            // the stated size and class.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    ptr::addr_of_mut!(info).cast(),
                    std::mem::size_of_val(&info) as u32,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(info.ActiveProcesses)
        }

        /// Collect stdout and stderr, wait, then kill any descendant still
        /// running so a background process holding a pipe cannot block the
        /// read forever.
        pub fn wait_with_output(mut self) -> io::Result<Output> {
            drop(self.stdin.take());
            let reader = |pipe: Option<Box<dyn Read + Send>>| {
                std::thread::spawn(move || -> io::Result<Vec<u8>> {
                    let mut buf = Vec::new();
                    if let Some(mut pipe) = pipe {
                        pipe.read_to_end(&mut buf)?;
                    }
                    Ok(buf)
                })
            };
            let out = reader(
                self.stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
            );
            let err = reader(
                self.stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
            );
            let status = self.wait()?;
            self.kill()?;
            let join = |h: std::thread::JoinHandle<io::Result<Vec<u8>>>| {
                h.join()
                    .map_err(|_| io::Error::other("pipe reader thread panicked"))?
            };
            Ok(Output {
                status,
                stdout: join(out)?,
                stderr: join(err)?,
            })
        }
    }

    /// Owns a PROC_THREAD_ATTRIBUTE_LIST and the handle array it points to.
    /// Also used by the sandbox user's runner (`sandbox_user::windows::runner`).
    pub(crate) struct AttributeList {
        buf: Vec<u64>,
        _handles: Box<[HANDLE]>,
    }

    impl AttributeList {
        /// A list with one attribute: the exact set of handles to inherit.
        pub(crate) fn with_handles(handles: Vec<HANDLE>) -> Result<Self, SandboxError> {
            let mut size = 0usize;
            // SAFETY: size query with a null list.
            unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
            if size == 0 {
                return Err(last_error("InitializeProcThreadAttributeList size"));
            }
            let mut list = Self {
                buf: vec![0u64; size.div_ceil(8)],
                _handles: handles.into_boxed_slice(),
            };
            // SAFETY: `buf` holds `size` bytes.
            if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), 1, 0, &mut size) } == 0 {
                // Not initialized: Drop must not delete it.
                list.buf.clear();
                return Err(last_error("InitializeProcThreadAttributeList"));
            }
            // SAFETY: the boxed slice has a stable address for the life of
            // the list, and the size covers it.
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    list.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    list._handles.as_ptr().cast(),
                    std::mem::size_of_val(&*list._handles),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if ok == 0 {
                return Err(last_error("UpdateProcThreadAttribute(handle list)"));
            }
            Ok(list)
        }

        pub(crate) fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buf.as_mut_ptr().cast()
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.buf.is_empty() {
                // SAFETY: an initialized list, deleted once.
                unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
            }
        }
    }

    /// Start `cmd` under the prepared restrictions. `stdio` is stdin, stdout,
    /// stderr.
    pub(in crate::sandbox) fn spawn(
        cmd: &Command,
        prepared: &Prepared,
        stdio: [Redirect; 3],
        inherit_env: bool,
    ) -> Result<SandboxedChild, SandboxError> {
        let cwd = cmd.get_current_dir().ok_or_else(|| {
            SandboxError::InvalidPolicy("sandboxed command has no working directory".into())
        })?;
        // `sh` and `bash` run through Git Bash (see `SHELL_TRAMPOLINE`).
        let launch = match cmd.get_program().to_str().and_then(posix_shell) {
            Some(shell) => Some(git_bash_launch(shell, cmd)?),
            None => None,
        };
        let (env, path) = final_env(
            cmd,
            launch
                .as_ref()
                .map(|l| (SHELL_COMMAND_VAR, l.command.as_str())),
            prepared.user.as_ref().map_or(&[][..], |u| &u.env[..]),
            inherit_env,
        );
        let program = match &launch {
            Some(l) => l.program.clone(),
            None => resolve_program(cmd.get_program(), cwd, path.as_deref())?,
        };
        let program_text = program.to_string_lossy();
        if is_batch_file(&program_text) {
            return Err(SandboxError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch files are refused: cmd.exe parses their arguments differently; \
                 run cmd.exe /d /c explicitly",
            )));
        }
        if program_text.contains('"') {
            return Err(SandboxError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                "program path contains a quote",
            )));
        }
        // argv[0] is always quoted and never escaped, as std does.
        let mut line = match &launch {
            Some(l) => command_line(
                &l.program.as_os_str().encode_wide().collect::<Vec<_>>(),
                l.args.iter().map(|a| a.encode_utf16().collect::<Vec<_>>()),
            ),
            None => command_line(
                &cmd.get_program().encode_wide().collect::<Vec<_>>(),
                cmd.get_args().map(|a| a.encode_wide().collect::<Vec<_>>()),
            ),
        };
        if let Some(user) = &prepared.user {
            return spawn_as_user(user, &program, line, cwd, env, stdio);
        }
        line.push(0);

        // The desktop first: its SID is one of the token's restricting SIDs.
        let desktop = desktop_for(&prepared.desktop_key)?;
        let token = build_sandbox_token(&prepared.caps, Some(&desktop.sid))?;
        let job = create_job(JOB_OBJECT_UILIMIT_ALL)?;
        let [stdin, stdout, stderr] = [
            stream(STD_INPUT_HANDLE, stdio[0])?,
            stream(STD_OUTPUT_HANDLE, stdio[1])?,
            stream(STD_ERROR_HANDLE, stdio[2])?,
        ];
        let raw = |s: &Stream| {
            s.child
                .as_ref()
                .map_or(ptr::null_mut(), |h| h.as_raw_handle())
        };
        let handles: Vec<HANDLE> = [&stdin, &stdout, &stderr]
            .iter()
            .filter_map(|s| s.child.as_ref().map(|h| h.as_raw_handle()))
            .collect();
        // Inherit exactly the intended handles through the handle list, and
        // never rely on global inheritance: bInheritHandles is TRUE only with a
        // handle list present, and FALSE otherwise. The listed handles
        // must be marked inheritable, so a concurrent CreateProcess elsewhere
        // in this process that passes bInheritHandles=TRUE without a list could
        // still inherit them during the brief window they are open; std has the
        // same race, and the handle list is the accepted mitigation.
        let inherit = !handles.is_empty();
        let mut attrs = if inherit {
            Some(AttributeList::with_handles(handles)?)
        } else {
            None
        };

        // SAFETY: zeroed is a valid initial STARTUPINFOEXW.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.lpDesktop = desktop.startup_name.as_ptr().cast_mut();
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = raw(&stdin);
        si.StartupInfo.hStdOutput = raw(&stdout);
        si.StartupInfo.hStdError = raw(&stderr);
        if let Some(attrs) = attrs.as_mut() {
            si.lpAttributeList = attrs.as_ptr();
        }
        let app = wide(program.as_os_str());
        let cwd_w = wide(cwd.as_os_str());
        // SAFETY: zeroed is a valid initial PROCESS_INFORMATION.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // CREATE_NO_WINDOW: no console is attached, so the child cannot reach
        // the user's console (the Windows counterpart of `setsid` on Unix).
        // CREATE_SUSPENDED: it joins the job before running any code.
        let flags = CREATE_SUSPENDED
            | CREATE_UNICODE_ENVIRONMENT
            | CREATE_NO_WINDOW
            | if inherit {
                EXTENDED_STARTUPINFO_PRESENT
            } else {
                0
            };
        // SAFETY: every pointer is NUL-terminated or a valid struct that
        // outlives the call; `line` is mutable as the API requires.
        let ok = unsafe {
            CreateProcessAsUserW(
                token.as_raw_handle(),
                app.as_ptr(),
                line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                i32::from(inherit),
                flags,
                env.as_ptr().cast(),
                cwd_w.as_ptr(),
                ptr::addr_of!(si.StartupInfo),
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(SandboxError::Spawn(io::Error::last_os_error()));
        }
        let process = owned(pi.hProcess, "CreateProcessAsUserW process")?;
        let thread = owned(pi.hThread, "CreateProcessAsUserW thread")?;
        // SAFETY: valid job and process handles.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
            let err = last_error("AssignProcessToJobObject");
            // SAFETY: the suspended process never ran; terminate it.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(err);
        }
        // SAFETY: the primary thread handle of the suspended process.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            let err = last_error("ResumeThread");
            // SAFETY: valid job handle.
            unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
            return Err(err);
        }
        drop(attrs);
        Ok(SandboxedChild {
            stdin: stdin.parent.map(ChildStdin::from),
            stdout: stdout.parent.map(ChildStdout::from),
            stderr: stderr.parent.map(ChildStderr::from),
            process,
            pid: pi.dwProcessId,
            job,
            _desktop: Some(DesktopGuard(desktop)),
            _slot: None,
        })
    }

    /// Whether this process's standard stream `which` is a console.
    fn is_console(which: STD_HANDLE) -> bool {
        // SAFETY: plain call; the handle is borrowed, not owned.
        let h = unsafe { GetStdHandle(which) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode = 0;
        // SAFETY: probes whether `h` is a console handle.
        let ok = unsafe { GetConsoleMode(h, &mut mode) };
        ok != 0
    }

    /// A stream for a command that runs as the sandbox user, with one
    /// difference from [`stream`]: an inherited console handle is not given
    /// to the command. The command writes to a pipe, and a thread of this
    /// process copies what arrives to this process's own stream.
    ///
    /// A console handle carries more than writing text. Its holder can read
    /// what the window shows and add to its input, and a handle passed to a
    /// child is admitted without any access check, so neither the sandbox
    /// user's own list nor the deny-only logon SID would stop it.
    fn user_stream(which: STD_HANDLE, redirect: Redirect) -> Result<Stream, SandboxError> {
        if redirect != Redirect::Inherit || which == STD_INPUT_HANDLE || !is_console(which) {
            return stream(which, redirect);
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (mut read, mut write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
        // SAFETY: both outputs are written on success; `sa` lives across the
        // call. An anonymous pipe, not the overlapped pipe `child_pipe`
        // makes: the copy below reads it with ordinary blocking reads.
        if unsafe { CreatePipe(&mut read, &mut write, &sa, PIPE_BUFFER) } == 0 {
            return Err(last_error("CreatePipe(console relay)"));
        }
        let read = owned(read, "CreatePipe(console relay) read")?;
        let write = owned(write, "CreatePipe(console relay) write")?;
        // The command inherits the writing end only.
        // SAFETY: a valid handle of this process.
        if unsafe { SetHandleInformation(read.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last_error("SetHandleInformation(console relay)"));
        }
        let mut from = std::fs::File::from(read);
        std::thread::spawn(move || {
            // Ends when the command's last copy of the writing end closes.
            let _ = if which == STD_ERROR_HANDLE {
                io::copy(&mut from, &mut io::stderr())
            } else {
                io::copy(&mut from, &mut io::stdout())
            };
        });
        Ok(Stream {
            child: Some(write),
            parent: None,
        })
    }

    /// Start a command as the sandbox user: the runner starts as that user
    /// in a new job and starts the command (see `sandbox_user::windows::launch`).
    /// `line` is the command line without its terminating NUL; `env` is a
    /// complete environment block.
    fn spawn_as_user(
        user: &SandboxUser,
        program: &Path,
        line: Vec<u16>,
        cwd: &Path,
        env: Vec<u16>,
        stdio: [Redirect; 3],
    ) -> Result<SandboxedChild, SandboxError> {
        let request = Request {
            application: program.as_os_str().encode_wide().collect(),
            command_line: line,
            directory: strip_verbatim(cwd).as_os_str().encode_wide().collect(),
            environment: env,
        };
        // No UI limits. Windows nests a job inside another only if neither
        // sets them, and the runner may already be in a job (the secondary
        // logon service's, or one kGraf itself runs in). The runner and the
        // command stay in this job: kill on close, no breakaway. The
        // command's UI isolation is its own window station and desktop
        // (`sandbox_user::windows::runner`).
        let job = create_job(0)?;
        let [stdin, stdout, stderr] = [
            user_stream(STD_INPUT_HANDLE, stdio[0])?,
            user_stream(STD_OUTPUT_HANDLE, stdio[1])?,
            user_stream(STD_ERROR_HANDLE, stdio[2])?,
        ];
        let raw = |s: &Stream| {
            s.child
                .as_ref()
                .map_or(ptr::null_mut(), |h| h.as_raw_handle())
        };
        let started = launch::start(
            &launch::Credential {
                name: &user.user.name,
                sid: &user.user.sid,
                password: &user.user.password,
            },
            user.runner,
            [raw(&stdin), raw(&stdout), raw(&stderr)],
            &job,
            &request,
        )?;
        Ok(SandboxedChild {
            stdin: stdin.parent.map(ChildStdin::from),
            stdout: stdout.parent.map(ChildStdout::from),
            stderr: stderr.parent.map(ChildStderr::from),
            process: started.runner,
            pid: started.command_pid,
            job,
            _desktop: None,
            _slot: Some(user.slot.clone()),
        })
    }

    pub(in crate::sandbox) fn output(
        cmd: &Command,
        prepared: &Prepared,
        stdio: [Option<Redirect>; 3],
        inherit_env: bool,
    ) -> Result<Output, SandboxError> {
        let child = spawn(
            cmd,
            prepared,
            [
                stdio[0].unwrap_or(Redirect::Null),
                stdio[1].unwrap_or(Redirect::Piped),
                stdio[2].unwrap_or(Redirect::Piped),
            ],
            inherit_env,
        )?;
        child.wait_with_output().map_err(SandboxError::Spawn)
    }

    pub(in crate::sandbox) fn status(
        cmd: &Command,
        prepared: &Prepared,
        stdio: [Option<Redirect>; 3],
        inherit_env: bool,
    ) -> Result<ExitStatus, SandboxError> {
        let mut child = spawn(cmd, prepared, default_stdio(stdio), inherit_env)?;
        child.wait().map_err(SandboxError::Spawn)
    }

    /// Defaults for `spawn` and `status`: stdin is null (an inherited console
    /// input handle is refused, see [`stream`]); stdout and stderr inherit.
    pub(in crate::sandbox) fn default_stdio(stdio: [Option<Redirect>; 3]) -> [Redirect; 3] {
        [
            stdio[0].unwrap_or(Redirect::Null),
            stdio[1].unwrap_or(Redirect::Inherit),
            stdio[2].unwrap_or(Redirect::Inherit),
        ]
    }

    /// Which token property makes the Windows TLS stack (Schannel) refuse a
    /// client credential. Schannel's `AcquireCredentialsHandle` is served
    /// inside LSA, which reads the calling thread's token, so a thread that
    /// impersonates a token gets the answer a process running with that
    /// token gets. No network and no server: the refusal comes before any
    /// connection. See "TLS through Schannel" at the top of this file.
    #[cfg(test)]
    mod schannel {
        use super::*;
        use windows_sys::Win32::Foundation::{SEC_E_NO_CREDENTIALS, SEC_E_OK};
        use windows_sys::Win32::Security::Authentication::Identity::{
            AcquireCredentialsHandleW, FreeCredentialsHandle, SCH_CRED_MANUAL_CRED_VALIDATION,
            SCH_CRED_NO_DEFAULT_CREDS, SCHANNEL_CRED, SCHANNEL_CRED_VERSION, SECPKG_CRED_OUTBOUND,
            UNISP_NAME_W,
        };
        use windows_sys::Win32::Security::Credentials::SecHandle;
        use windows_sys::Win32::Security::{RevertToSelf, TokenPrimary};
        use windows_sys::Win32::System::Threading::SetThreadToken;

        fn hex(status: i32) -> String {
            format!("{:#010x}", status as u32)
        }

        /// The status of an outbound Schannel credential request from a new
        /// thread that impersonates `token`. The request is the one curl's
        /// Schannel backend makes: no client certificate, server checked by
        /// the caller.
        fn client_credential_status(token: &OwnedHandle) -> i32 {
            // A HANDLE is a pointer and not `Send`; the thread is joined
            // before `token` can close.
            let raw = token.as_raw_handle() as usize;
            std::thread::spawn(move || {
                let mut imp: HANDLE = ptr::null_mut();
                // SAFETY: `raw` is a token handle that stays open until this
                // thread is joined; the output is written on success.
                let ok = unsafe {
                    DuplicateTokenEx(
                        raw as HANDLE,
                        TOKEN_QUERY | TOKEN_IMPERSONATE,
                        ptr::null(),
                        SecurityImpersonation,
                        TokenImpersonation,
                        &mut imp,
                    )
                };
                assert_ne!(ok, 0, "DuplicateTokenEx: {}", io::Error::last_os_error());
                let imp = owned(imp, "DuplicateTokenEx").unwrap();
                // SAFETY: a null thread pointer means the calling thread;
                // `imp` is a valid impersonation token.
                let ok = unsafe { SetThreadToken(ptr::null(), imp.as_raw_handle()) };
                assert_ne!(ok, 0, "SetThreadToken: {}", io::Error::last_os_error());
                let cred = SCHANNEL_CRED {
                    dwVersion: SCHANNEL_CRED_VERSION,
                    dwFlags: SCH_CRED_NO_DEFAULT_CREDS | SCH_CRED_MANUAL_CRED_VALIDATION,
                    ..Default::default()
                };
                let mut handle = SecHandle {
                    dwLower: 0,
                    dwUpper: 0,
                };
                let mut expiry = 0i64;
                // SAFETY: the package name is a static NUL-terminated string,
                // `cred` is a valid SCHANNEL_CRED, and both outputs are valid.
                let status = unsafe {
                    AcquireCredentialsHandleW(
                        ptr::null(),
                        UNISP_NAME_W,
                        SECPKG_CRED_OUTBOUND,
                        ptr::null(),
                        ptr::addr_of!(cred).cast(),
                        None,
                        ptr::null(),
                        &mut handle,
                        &mut expiry,
                    )
                };
                if status == SEC_E_OK {
                    // SAFETY: a credential handle acquired above, freed once.
                    unsafe { FreeCredentialsHandle(&handle) };
                }
                // SAFETY: ends this thread's impersonation; no arguments.
                unsafe { RevertToSelf() };
                status
            })
            .join()
            .unwrap()
        }

        /// A primary copy of this process's token, which can be changed
        /// without changing the process.
        pub(super) fn own_token_copy() -> OwnedHandle {
            let mut raw: HANDLE = ptr::null_mut();
            // SAFETY: pseudo-handle of this process; output written on success.
            let ok = unsafe {
                OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut raw)
            };
            assert_ne!(ok, 0, "OpenProcessToken: {}", io::Error::last_os_error());
            let base = owned(raw, "OpenProcessToken").unwrap();
            let mut dup: HANDLE = ptr::null_mut();
            // SAFETY: a valid token opened for duplication; output written on
            // success.
            let ok = unsafe {
                DuplicateTokenEx(
                    base.as_raw_handle(),
                    TOKEN_DUPLICATE
                        | TOKEN_QUERY
                        | TOKEN_IMPERSONATE
                        | TOKEN_ADJUST_DEFAULT
                        | TOKEN_ASSIGN_PRIMARY,
                    ptr::null(),
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut dup,
                )
            };
            assert_ne!(ok, 0, "DuplicateTokenEx: {}", io::Error::last_os_error());
            owned(dup, "DuplicateTokenEx").unwrap()
        }

        /// The control: without it a refusal below could be a broken probe.
        #[test]
        fn schannel_gives_the_users_own_token_a_client_credential() {
            let status = client_credential_status(&own_token_copy());
            assert_eq!(status, SEC_E_OK, "got {}", hex(status));
        }

        /// The cause, pinned: the token every sandboxed command runs with is
        /// refused, so Schannel HTTPS (curl.exe, PowerShell, .NET, git with
        /// its Schannel backend) fails in the sandbox with
        /// `SEC_E_NO_CREDENTIALS` before it sends a byte. A change that makes
        /// Schannel work in the sandbox makes this test fail; it must then
        /// assert `SEC_E_OK`.
        #[test]
        fn schannel_refuses_the_sandbox_token_a_client_credential() {
            let token = build_sandbox_token(&[], None).unwrap();
            let status = client_credential_status(&token);
            assert_eq!(status, SEC_E_NO_CREDENTIALS, "got {}", hex(status));
        }

        /// Restricting SIDs alone cause the refusal: no Low integrity level,
        /// no default DACL change, one restricting SID (Everyone).
        #[test]
        fn restricting_sids_alone_make_schannel_refuse() {
            let everyone = Sid::from_string("S-1-1-0").unwrap();
            let (token, _, _) = restricted_token_for(&[everyone]).unwrap();
            let status = client_credential_status(&token);
            assert_eq!(status, SEC_E_NO_CREDENTIALS, "got {}", hex(status));
        }

        /// The Low integrity level alone does not cause it: a token with no
        /// restricting SID, lowered to Low, still gets a credential. A write
        /// boundary built on the integrity level without restricting SIDs
        /// would therefore keep Schannel working.
        #[test]
        fn low_integrity_alone_keeps_schannel_working() {
            let token = own_token_copy();
            set_integrity_low(&token).unwrap();
            let status = client_credential_status(&token);
            assert_eq!(status, SEC_E_OK, "got {}", hex(status));
        }
    }

    /// Runs `f` on a new thread that impersonates `token`, and returns what
    /// it returns. Objects the thread creates get the token's default DACL
    /// and integrity label, and its access checks are the token's, as in a
    /// process running with that token.
    #[cfg(test)]
    fn impersonating<T: Send + 'static>(
        token: &OwnedHandle,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        use windows_sys::Win32::Security::RevertToSelf;
        use windows_sys::Win32::System::Threading::SetThreadToken;
        // A HANDLE is a pointer and not `Send`; the thread is joined before
        // `token` can close.
        let raw = token.as_raw_handle() as usize;
        std::thread::spawn(move || {
            let mut imp: HANDLE = ptr::null_mut();
            // SAFETY: `raw` is a token handle that stays open until this
            // thread is joined; the output is written on success.
            let ok = unsafe {
                DuplicateTokenEx(
                    raw as HANDLE,
                    TOKEN_QUERY | TOKEN_IMPERSONATE,
                    ptr::null(),
                    SecurityImpersonation,
                    TokenImpersonation,
                    &mut imp,
                )
            };
            assert_ne!(ok, 0, "DuplicateTokenEx: {}", io::Error::last_os_error());
            let imp = owned(imp, "DuplicateTokenEx").unwrap();
            // SAFETY: a null thread pointer means the calling thread; `imp`
            // is a valid impersonation token.
            let ok = unsafe { SetThreadToken(ptr::null(), imp.as_raw_handle()) };
            assert_ne!(ok, 0, "SetThreadToken: {}", io::Error::last_os_error());
            let out = f();
            // SAFETY: ends this thread's impersonation; no arguments.
            unsafe { RevertToSelf() };
            out
        })
        .join()
        .unwrap()
    }

    /// Why MSYS2 and Cygwin programs (Git Bash's `sh`, `bash` and tools) stop
    /// with STATUS_DLL_INIT_FAILED in this sandbox. While the runtime loads it
    /// creates its signal pipe with an ACL that grants `GENERIC_ALL` to the
    /// user SID, Administrators and SYSTEM only, then opens the pipe for
    /// `GENERIC_WRITE | FILE_READ_ATTRIBUTES` (msys2-runtime
    /// `winsup/cygwin/sigproc.cc` `sigproc_init`, `sec/helper.cc` `sec_acl`,
    /// `fhandler/pipe.cc` `fhandler_pipe::create`). Each test makes that pipe
    /// and that open on a thread impersonating one token, so the result is
    /// the one the runtime gets, with no Git for Windows needed.
    #[cfg(test)]
    mod msys_pipe {
        use super::*;
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows_sys::Win32::Security::{
            InitializeSecurityDescriptor, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_READ_ATTRIBUTES, PIPE_ACCESS_INBOUND,
        };
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
        };
        use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

        /// The runtime's two steps under `token`: `Ok(())` when the write
        /// open succeeds, `Err(code)` with the Win32 error of the open. A
        /// failure to create the pipe at all panics, naming that step.
        fn signal_pipe_open(token: &OwnedHandle) -> Result<(), u32> {
            let user = process_user_sid().unwrap();
            impersonating(token, move || {
                let admins = Sid::from_string("S-1-5-32-544").unwrap();
                let system = Sid::from_string("S-1-5-18").unwrap();
                let entries = [
                    explicit(&user, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
                    explicit(&admins, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
                    explicit(&system, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
                ];
                let mut acl: *mut ACL = ptr::null_mut();
                // SAFETY: valid entries, no old ACL; freed by `Local`.
                let code = unsafe {
                    SetEntriesInAclW(
                        entries.len() as u32,
                        entries.as_ptr(),
                        ptr::null(),
                        &mut acl,
                    )
                };
                assert_eq!(code, ERROR_SUCCESS, "SetEntriesInAclW");
                let _acl = Local(acl.cast());
                // SAFETY: zeroed is a valid value to initialize.
                let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
                let psd: PSECURITY_DESCRIPTOR = ptr::addr_of_mut!(sd).cast();
                // SAFETY: `psd` points at a SECURITY_DESCRIPTOR-sized buffer.
                let ok = unsafe { InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) };
                assert_ne!(ok, 0, "InitializeSecurityDescriptor");
                // SAFETY: an initialized descriptor and an ACL that outlives it.
                let ok = unsafe { SetSecurityDescriptorDacl(psd, 1, acl, 0) };
                assert_ne!(ok, 0, "SetSecurityDescriptorDacl");
                let sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: psd,
                    bInheritHandle: 0,
                };
                let name = wide(OsStr::new(&format!(
                    r"\\.\pipe\kgraf-msys-probe-{:016x}-sigwait",
                    random_u64()
                )));
                // SAFETY: NUL-terminated name and valid attributes.
                let server = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                        PIPE_TYPE_MESSAGE | PIPE_READMODE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
                        1,
                        4096,
                        4096,
                        0,
                        &sa,
                    )
                };
                let _server = owned(server, "CreateNamedPipeW").unwrap_or_else(|e| {
                    panic!("creating the pipe failed, before the write open: {e}")
                });
                // SAFETY: NUL-terminated name; no template, no attributes.
                let client = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_WRITE | FILE_READ_ATTRIBUTES,
                        0,
                        ptr::null(),
                        OPEN_EXISTING,
                        0,
                        ptr::null_mut(),
                    )
                };
                if client == INVALID_HANDLE_VALUE {
                    // SAFETY: plain call.
                    return Err(unsafe { GetLastError() });
                }
                drop(owned(client, "CreateFileW(pipe)"));
                Ok(())
            })
        }

        /// The control: the user's own token, lowered to Low as the sandbox
        /// token is, opens the pipe. Without it a refusal below could be the
        /// integrity level or a broken probe.
        #[test]
        fn a_low_copy_of_the_users_token_opens_the_msys_signal_pipe() {
            let token = super::schannel::own_token_copy();
            set_integrity_low(&token).unwrap();
            assert_eq!(signal_pipe_open(&token), Ok(()));
        }

        /// The cause, pinned: the token every sandboxed command runs with is
        /// refused the write open, because no restricting SID is in the
        /// pipe's ACL. A change that makes MSYS2 start in the sandbox makes
        /// this test fail; it must then assert `Ok(())`.
        #[test]
        fn the_sandbox_token_is_refused_the_msys_signal_pipe() {
            let token = build_sandbox_token(&[], None).unwrap();
            assert_eq!(signal_pipe_open(&token), Err(ERROR_ACCESS_DENIED));
        }

        /// The only change inside the token that opens it: the user SID as a
        /// restricting SID. The same SID then passes the restricting check on
        /// every object the user can write, which is why it is not done here
        /// (decision 0011, amendment "Desktop and environment").
        #[test]
        fn only_the_user_sid_as_a_restricting_sid_opens_the_msys_signal_pipe() {
            let user = process_user_sid().unwrap();
            let everyone = Sid::from_string("S-1-1-0").unwrap();
            let (token, _, _) = restricted_token_for(&[user, everyone]).unwrap();
            set_integrity_low(&token).unwrap();
            assert_eq!(signal_pipe_open(&token), Ok(()));
        }
    }

    /// Why programs that load `user32.dll` at start stopped with
    /// STATUS_DLL_INIT_FAILED, and that the desktop SID fixes it: the open of
    /// the private desktop with the rights `user32.dll` takes, on a thread
    /// impersonating the token a command on that desktop runs with.
    #[cfg(test)]
    mod desktop_access {
        use super::*;
        use windows_sys::Win32::System::StationsAndDesktops::OpenDesktopW;

        /// Whether a thread with `token` opens the desktop named `name` (on
        /// this process's window station) with [`DESKTOP_PARTICIPANT`].
        fn opens(token: &OwnedHandle, name: Vec<u16>) -> bool {
            impersonating(token, move || {
                // SAFETY: NUL-terminated name; the handle is closed below.
                let h = unsafe { OpenDesktopW(name.as_ptr(), 0, 0, DESKTOP_PARTICIPANT) };
                if h.is_null() {
                    return false;
                }
                // SAFETY: a desktop handle opened above, closed once.
                unsafe { CloseDesktop(h) };
                true
            })
        }

        /// The bare desktop name, without the window station prefix.
        fn bare_name(desktop: &Desktop) -> Vec<u16> {
            let full = &desktop.startup_name;
            let start = full
                .iter()
                .rposition(|c| *c == u16::from(b'\\'))
                .map_or(0, |i| i + 1);
            full[start..].to_vec()
        }

        /// A command's token, with the desktop's SID, opens the desktop with
        /// the participant rights; the same token without that SID (the
        /// backend before the fix) is refused, so the SID is what admits it.
        #[test]
        fn the_desktop_sid_admits_a_command_and_its_absence_refuses_it() {
            let desktop = desktop_for("desktop-access-test").unwrap();
            let with = build_sandbox_token(&[], Some(&desktop.sid)).unwrap();
            assert!(opens(&with, bare_name(&desktop)));
            let without = build_sandbox_token(&[], None).unwrap();
            assert!(!opens(&without, bare_name(&desktop)));
        }

        /// Another workspace's desktop does not admit this command: its SID
        /// is a different one.
        #[test]
        fn a_desktop_does_not_admit_another_desktops_command() {
            let mine = desktop_for("desktop-access-mine").unwrap();
            let other = desktop_for("desktop-access-other").unwrap();
            let token = build_sandbox_token(&[], Some(&mine.sid)).unwrap();
            assert!(!opens(&token, bare_name(&other)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let mut out = Vec::new();
        for a in args {
            crate::win_command_line::append_arg(&mut out, &a.encode_utf16().collect::<Vec<_>>());
        }
        String::from_utf16(&out).unwrap()
    }

    /// A hidden folder's writes are denied to the capability of each root
    /// that holds it, never to a root beneath it (the package store inside
    /// `.kgraf`), and not to an unrelated root.
    #[test]
    fn a_hidden_folder_denies_the_roots_above_it_only() {
        use std::path::PathBuf;
        // Built with joins, so the test reads the same on every platform.
        let base = PathBuf::from("/w");
        let project = base.join("p");
        let state = project.join(".kgraf");
        let store = state.join("package-stores");
        let roots = vec![project.clone(), store.clone(), base.join("other")];
        let caps = ["cap-p", "cap-store", "cap-other"];
        let hidden = [crate::sandbox::Hidden {
            dir: base.join("verbatim").join("p").join(".kgraf"),
            keep: vec![store.clone()],
        }];
        // Stands in for stripping a verbatim prefix: the given folder is
        // compared after it is normalized.
        let strip = |p: &std::path::Path| {
            p.strip_prefix(base.join("verbatim"))
                .map(|rest| base.join(rest))
                .unwrap_or_else(|_| p.to_path_buf())
        };
        let plan = hidden_denials(&hidden, &roots, &caps, strip);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, state);
        assert_eq!(plan[0].1, vec![&"cap-p"]);
    }

    /// Only the ACL check's own probe names are swept, and the process id
    /// is read from them exactly.
    #[test]
    fn only_acl_probe_names_are_recognized() {
        assert_eq!(acl_probe_pid(".kgraf-acl-probe-4242-0"), Some(4242));
        assert_eq!(acl_probe_pid(".kgraf-acl-probe-7-19"), Some(7));
        for other in [
            ".kgraf-acl-probe-",
            ".kgraf-acl-probe-42",
            ".kgraf-acl-probe-42-",
            ".kgraf-acl-probe-x-1",
            ".kgraf-acl-probe-42-1-extra",
            "kgraf-acl-probe-42-1",
            "src",
        ] {
            assert_eq!(acl_probe_pid(other), None, "{other}");
        }
    }

    #[test]
    fn arguments_follow_msvc_quoting() {
        assert_eq!(line(&["a", "b c"]), r#"a "b c""#);
        assert_eq!(line(&[""]), r#""""#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        // Backslashes are literal unless they precede a quote.
        assert_eq!(line(&[r"C:\dir\"]), r"C:\dir\");
        assert_eq!(line(&[r"C:\my dir\"]), r#""C:\my dir\\""#);
        assert_eq!(line(&[r#"a\"b"#]), r#"a\\\"b"#);
    }

    #[test]
    fn capability_sids_are_stable_distinct_and_case_insensitive() {
        let a = capability_sid_string(r"C:\ws\one");
        assert_eq!(a, capability_sid_string(r"C:\WS\ONE"));
        assert_ne!(a, capability_sid_string(r"C:\ws\two"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(&parts[..4], &["S", "1", "5", "21"]);
        assert_eq!(parts.len(), 8);
        assert!(parts[4..].iter().all(|p| p.parse::<u32>().is_ok()), "{a}");
    }

    #[test]
    fn batch_files_are_detected() {
        assert!(is_batch_file(r"C:\x\build.BAT"));
        assert!(is_batch_file("run.cmd"));
        assert!(!is_batch_file("cargo.exe"));
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn env_block_overrides_and_removes_without_case() {
        let block = env_block(
            vec![
                (w("Path"), w("C:\\bin")),
                (w("GITHUB_TOKEN"), w("secret")),
                (w("b"), w("1")),
            ],
            vec![(w("PATH"), Some(w("C:\\new"))), (w("github_token"), None)],
        );
        let text = String::from_utf16(&block).unwrap();
        assert_eq!(text, "b=1\0PATH=C:\\new\0\0");
    }

    #[test]
    fn empty_env_block_is_two_nuls() {
        assert_eq!(env_block(vec![], vec![]), vec![0, 0]);
    }

    /// The bare names and POSIX paths of `sh` and `bash` go to Git Bash; a
    /// Windows path to one particular `bash.exe`, and every other shell, run
    /// as given.
    #[test]
    fn only_sh_and_bash_by_name_or_posix_path_go_to_git_bash() {
        for (program, shell) in [
            ("sh", "sh"),
            ("bash", "bash"),
            ("SH.EXE", "sh"),
            ("bash.exe", "bash"),
            ("/bin/sh", "sh"),
            ("/usr/bin/sh", "sh"),
            ("/bin/bash", "bash"),
            ("/usr/bin/bash", "bash"),
        ] {
            assert_eq!(posix_shell(program), Some(shell), "{program}");
        }
        for program in [
            "zsh",
            "dash",
            "pwsh",
            "cmd",
            "shx",
            "",
            r"C:\Program Files\Git\bin\bash.exe",
            "/usr/local/bin/bash",
        ] {
            assert_eq!(posix_shell(program), None, "{program}");
        }
    }

    #[test]
    fn a_posix_word_is_single_quoted_with_each_quote_escaped() {
        assert_eq!(posix_quote("plain"), "'plain'");
        assert_eq!(posix_quote(""), "''");
        assert_eq!(posix_quote("it's"), r"'it'\''s'");
        assert_eq!(posix_quote(r#"$HOME `x` \\ "q""#), r#"'$HOME `x` \\ "q"'"#);
    }

    /// The trampoline holds no quote and no backslash, the characters MSVC
    /// and MSYS2 parse differently, so on the command line it is one word
    /// under both: the MSVC writer puts it in plain double quotes, and the
    /// MSYS2 reader takes a double-quoted run with nothing inside it to
    /// interpret.
    #[test]
    fn the_trampoline_is_one_word_for_both_command_line_parsers() {
        assert!(
            !SHELL_TRAMPOLINE.contains(['"', '\'', '\\']),
            "{SHELL_TRAMPOLINE}"
        );
        assert!(SHELL_TRAMPOLINE.ends_with(&format!("eval ${SHELL_COMMAND_VAR}")));
        assert_eq!(
            line(&["-c", SHELL_TRAMPOLINE]),
            r#"-c "IFS=;set -f;eval $KGRAF_SHELL_COMMAND""#
        );
    }

    #[test]
    fn a_shell_launch_runs_git_bash_with_the_trampoline_and_carries_the_argv() {
        let args = [
            std::ffi::OsStr::new("-c"),
            std::ffi::OsStr::new("echo 'a b'"),
        ];
        let launch = shell_launch("sh", &args, "G:/Git/bin/bash.exe".into()).unwrap();
        assert_eq!(
            launch.program,
            std::path::PathBuf::from("G:/Git/bin/bash.exe")
        );
        assert_eq!(launch.args, ["-c", SHELL_TRAMPOLINE]);
        assert_eq!(
            launch.command,
            r"unset KGRAF_SHELL_COMMAND; set +f; IFS=$' \t\n'; exec '/usr/bin/sh' '-c' 'echo '\''a b'\'''"
        );
    }

    #[test]
    fn a_shell_launch_refuses_what_an_environment_value_cannot_carry() {
        let bash = || std::path::PathBuf::from("G:/Git/bin/bash.exe");
        let nul = shell_launch("sh", &[std::ffi::OsStr::new("a\0b")], bash()).unwrap_err();
        assert!(nul.contains("NUL"), "{nul}");
        let long = "x".repeat(MAX_ENV_VALUE);
        let err = shell_launch("bash", &[std::ffi::OsStr::new(&long)], bash()).unwrap_err();
        assert!(err.contains("at most 32766"), "{err}");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let err =
                shell_launch("sh", &[std::ffi::OsStr::from_bytes(b"\xff")], bash()).unwrap_err();
            assert!(err.contains("not valid Unicode"), "{err}");
        }
    }

    /// The transport itself, run by a real bash on this host: the trampoline
    /// on the command line, the quoted argv in the variable. Each argument
    /// arrives byte for byte (spaces, quotes, backslashes, a glob that
    /// matches files, braces, `~`, `@`, a newline, an empty one), the
    /// variable is gone, globbing works again inside the script, and the
    /// exit status comes back. Only the first hop differs on Windows: Git
    /// Bash reads the trampoline from a command line, which the test above
    /// pins.
    #[cfg(unix)]
    #[test]
    fn the_trampoline_hands_the_exact_argv_to_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        let script = r#"printf '<%s>' "$0"; for a; do printf '[%s]' "$a"; done; printf '{%s}' "${KGRAF_SHELL_COMMAND-unset}"; echo; echo *.rs; exit 7"#;
        let args = [
            "-c",
            script,
            "zero",
            "a b",
            "it's",
            "*.rs",
            r"C:\\dir\\",
            r#""q""#,
            "{x,y}",
            "~",
            "@file",
            "new\nline",
            "",
        ];
        let text = shell_command_text("/bin/sh", &args);
        let out = std::process::Command::new("/bin/bash")
            .args(["-c", SHELL_TRAMPOLINE])
            .env(SHELL_COMMAND_VAR, &text)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "<zero>[a b][it's][*.rs][C:\\\\dir\\\\][\"q\"][{x,y}][~][@file][new\nline][]{unset}\n\
             a.rs b.rs\n"
        );
    }

    fn lookup<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
        entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// With the network open the sandbox gives the credential-helper reset,
    /// the OpenSSL selection and the line-ending settings, from index zero.
    #[test]
    fn git_config_entries_start_at_zero_with_the_network_open() {
        let (entries, count) = super::super::git_config_entries(&git_config_pairs(true));
        assert_eq!(count, 5);
        assert_eq!(
            lookup(&entries, "GIT_CONFIG_KEY_0"),
            Some("credential.helper")
        );
        assert_eq!(lookup(&entries, "GIT_CONFIG_VALUE_0"), Some(""));
        assert_eq!(
            lookup(&entries, "GIT_CONFIG_KEY_1"),
            Some("http.sslBackend")
        );
        assert_eq!(lookup(&entries, "GIT_CONFIG_VALUE_1"), Some("openssl"));
        assert_eq!(lookup(&entries, "GIT_CONFIG_KEY_2"), Some("core.autocrlf"));
        assert_eq!(lookup(&entries, "GIT_CONFIG_VALUE_2"), Some("false"));
        assert_eq!(lookup(&entries, "GIT_CONFIG_KEY_3"), Some("core.eol"));
        assert_eq!(lookup(&entries, "GIT_CONFIG_VALUE_3"), Some("lf"));
        assert_eq!(lookup(&entries, "GIT_CONFIG_KEY_4"), Some("core.safecrlf"));
        assert_eq!(entries.len(), 10);
    }

    /// The Windows tool settings applied over a host that hands git a
    /// credential through `GIT_CONFIG_COUNT`, through the same environment
    /// block a Windows spawn builds: every host entry name is removed,
    /// whatever its case, and the command's entries are the sandbox's own
    /// from index zero, with a count that matches them. The value is a fake.
    #[test]
    fn the_windows_tool_env_drops_the_hosts_git_config_entries() {
        let host = [
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "http.https://example.com/.extraheader"),
            (
                "GIT_CONFIG_VALUE_0",
                "AUTHORIZATION: bearer fake-test-value",
            ),
            ("Git_Config_Key_1", "user.name"),
            ("git_config_value_1", "Preset"),
            (
                "GIT_CONFIG_PARAMETERS",
                "'http.extraheader'='fake-test-value'",
            ),
            ("Path", r"C:\Windows"),
        ];
        let changes = super::super::worker_git_config_env(
            host.iter().map(|(k, _)| std::ffi::OsString::from(k)),
            tool_env(true),
        );
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        let inherited = host.iter().map(|(k, v)| (wide(k), wide(v))).collect();
        let overrides = changes
            .iter()
            .map(|(k, v)| {
                (
                    wide(&k.to_string_lossy()),
                    v.as_ref().map(|v| wide(&v.to_string_lossy())),
                )
            })
            .collect();
        let block = String::from_utf16_lossy(&env_block(inherited, overrides));
        let vars: Vec<(String, String)> = block
            .split('\0')
            .filter_map(|e| e.split_once('='))
            .map(|(k, v)| (k.to_uppercase(), v.to_owned()))
            .collect();
        assert!(!block.to_lowercase().contains("extraheader"), "{block}");
        assert!(!block.contains("fake-test-value"), "{block}");
        assert!(!block.contains("Preset"), "{block}");
        assert_eq!(lookup(&vars, "GIT_CONFIG_PARAMETERS"), None, "{block}");
        assert_eq!(lookup(&vars, "GIT_CONFIG_COUNT"), Some("5"), "{block}");
        assert_eq!(lookup(&vars, "GIT_CONFIG_KEY_0"), Some("credential.helper"));
        assert_eq!(lookup(&vars, "GIT_CONFIG_KEY_2"), Some("core.autocrlf"));
        assert_eq!(lookup(&vars, "GIT_CONFIG_VALUE_2"), Some("false"));
        let count = |prefix: &str| vars.iter().filter(|(k, _)| k.starts_with(prefix)).count();
        assert_eq!(count("GIT_CONFIG_KEY_"), 5, "{block}");
        assert_eq!(count("GIT_CONFIG_VALUE_"), 5, "{block}");
        assert_eq!(lookup(&vars, "PATH"), Some(r"C:\Windows"));
    }

    /// Without an open network the credential-helper reset and the
    /// line-ending settings are given, and no TLS backend.
    #[test]
    fn git_config_entries_keep_the_tls_backend_out_without_open_network() {
        let (entries, count) = super::super::git_config_entries(&git_config_pairs(false));
        assert_eq!(count, 4);
        assert_eq!(lookup(&entries, "GIT_CONFIG_KEY_1"), Some("core.autocrlf"));
        assert_eq!(
            lookup(&entries, "GIT_CONFIG_KEY_0"),
            Some("credential.helper")
        );
        assert!(entries.iter().all(|(_, v)| v != "openssl"));
    }

    /// The config entries never name the variables that freeze git's own
    /// configuration, so their effect is intact.
    #[test]
    fn git_config_entries_leave_the_frozen_config_variables_alone() {
        let (entries, _) = super::super::git_config_entries(&git_config_pairs(true));
        assert!(entries.iter().all(|(k, _)| {
            k.starts_with("GIT_CONFIG_KEY_") || k.starts_with("GIT_CONFIG_VALUE_")
        }));
    }

    fn os(pairs: &[(&str, &str)]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    /// A command whose environment was cleared inherits the base variables
    /// only, matched without case as Windows matches names; an API key and a
    /// plain variable stay behind. A command that was not cleared inherits
    /// everything, as before (the scrub then removes credential names).
    #[test]
    fn a_cleared_environment_inherits_only_the_base_variables() {
        let parent = os(&[
            ("SystemRoot", r"C:\Windows"),
            ("Path", r"C:\Windows\System32"),
            ("userprofile", r"C:\Users\u"),
            ("ANTHROPIC_API_KEY", "sk-should-not-leak"),
            ("KGRAF_PLAIN", "visible"),
            ("TEMP", r"C:\Users\u\AppData\Local\Temp"),
        ]);
        let kept = inherited_vars(parent.clone(), false);
        assert_eq!(
            kept,
            os(&[
                ("SystemRoot", r"C:\Windows"),
                ("Path", r"C:\Windows\System32"),
                ("userprofile", r"C:\Users\u"),
            ])
        );
        assert_eq!(inherited_vars(parent.clone(), true), parent);
    }

    /// The variables the sandbox sets for the tools: the git entries from
    /// zero, the count after them, and the curl backend only with the
    /// network open.
    #[test]
    fn tool_env_is_the_git_entries_the_count_and_the_curl_backend() {
        let open = tool_env(true);
        assert_eq!(lookup(&open, "GIT_CONFIG_KEY_0"), Some("credential.helper"));
        assert_eq!(lookup(&open, "GIT_CONFIG_KEY_1"), Some("http.sslBackend"));
        assert_eq!(lookup(&open, "GIT_CONFIG_COUNT"), Some("5"));
        assert_eq!(lookup(&open, CURL_SSL_BACKEND_VAR), Some(TLS_BACKEND));
        let closed = tool_env(false);
        assert_eq!(lookup(&closed, "GIT_CONFIG_COUNT"), Some("4"));
        assert_eq!(lookup(&closed, CURL_SSL_BACKEND_VAR), None);
    }

    /// After a clear the git entries start at zero, as without one, the
    /// TLS settings survive, and `TMP` and `TEMP` are the policy's folder.
    #[test]
    fn a_clear_keeps_the_sandbox_settings_numbered_from_zero() {
        let tmp = std::path::Path::new(r"C:\ws\scratch");
        let kept: Vec<(String, String)> = kept_on_clear(true, Some(tmp))
            .into_iter()
            .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(lookup(&kept, "GIT_CONFIG_KEY_0"), Some("credential.helper"));
        assert_eq!(lookup(&kept, "GIT_CONFIG_VALUE_0"), Some(""));
        assert_eq!(lookup(&kept, "GIT_CONFIG_KEY_1"), Some("http.sslBackend"));
        assert_eq!(lookup(&kept, "GIT_CONFIG_KEY_2"), Some("core.autocrlf"));
        assert_eq!(lookup(&kept, "GIT_CONFIG_COUNT"), Some("5"));
        assert_eq!(lookup(&kept, CURL_SSL_BACKEND_VAR), Some("openssl"));
        assert_eq!(lookup(&kept, "TMP"), Some(r"C:\ws\scratch"));
        assert_eq!(lookup(&kept, "TEMP"), Some(r"C:\ws\scratch"));
        let bare: Vec<String> = kept_on_clear(false, None)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            bare,
            [
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
                "GIT_CONFIG_KEY_1",
                "GIT_CONFIG_VALUE_1",
                "GIT_CONFIG_KEY_2",
                "GIT_CONFIG_VALUE_2",
                "GIT_CONFIG_KEY_3",
                "GIT_CONFIG_VALUE_3",
                "GIT_CONFIG_COUNT"
            ]
        );
    }
}
