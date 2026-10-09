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
//! profiles and the Credential Manager API stay readable. Process objects
//! carry a no-read-up label, so a Low process cannot open the Medium harness
//! process for `PROCESS_VM_READ` (measured: "Access is denied"). Network egress
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
           the user can read stay readable; network egress is not blocked; a DACL \
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
           SID is granted; a file in a writable root that has another hard link keeps the \
           access list and the integrity label it has, because every hard link of a file \
           shares one security descriptor, so a command cannot write such a file in its \
           own tree and a build that writes one in place fails with an access error; the \
           gitlink deny, the Low label on the writable roots, and cross-sandbox write \
           denial are unverified on Windows",
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
    // The DNS clause stays until a Windows run proves the DNS filter: the
    // proof is the `dns:` line of `kgraf sandbox selfcheck`. Where Windows
    // refuses the filter at setup, the gap stays for good on that machine.
    gaps: "the sandbox user can read whatever every local user can read (other drives, \
           Program Files, world-readable folders), though not the owner's profile; it can \
           still write where every local user may write below the folders setup closes to \
           it (subfolders of ProgramData and Public that already grant every user write, a \
           drive attached after setup, and TrustedInstaller's folders under the Windows \
           folder, such as System32\\Tasks and System32\\spool\\drivers\\color), and in its \
           own profile, which keeps files from one command to the next; registry keys and \
           named objects every user may write are not closed; DNS lookups go through the \
           system's DNS client service, which the firewall rules do not scope to the \
           sandbox user: `kgraf sandbox setup` adds WFP filters meant to refuse that \
           service to the slot users without the network, but no Windows run has proved \
           them, so until `kgraf sandbox selfcheck` prints `dns: confined` on this machine \
           a name lookup can leave the machine; Windows can refuse the filter at the name \
           resolution cache layers (Windows Server 2025 refused it with \
           FWP_E_OUT_OF_BOUNDS), and setup then goes on without it, says at its end that \
           DNS is not confined on this machine, and `kgraf sandbox status` shows the filter \
           as FAILED; where Windows takes it, it is not known whether a block at the name \
           resolution cache layers makes a lookup fail or only keeps the service from its \
           cache while the query still leaves the machine, because Microsoft documents no \
           effect for a block there; a command's launch does not check that the filters are \
           still on the machine, because reading a Windows Filtering Platform object can \
           need administrator rights and a launch is a standard user's, so commands run as \
           the sandbox user with whatever filters the machine has, and only \
           `kgraf sandbox status` and `kgraf sandbox selfcheck` show which; \
           loopback is closed by WFP filters, not by the firewall rules, which do not act \
           on it: a command without the network reaches no loopback port outside kGraf's \
           egress proxy ports, a listener it started itself included, so a test that starts \
           a local server and connects to it fails there; where Windows refuses a loopback \
           filter at setup, loopback is not confined on this machine and \
           `kgraf sandbox status` shows the filter as FAILED; \
           a program that creates a named object in the session is refused, because the \
           session admits its logon SID, which the command holds deny-only: cargo and \
           rustc get a jobserver kGraf makes for each command instead, other programs \
           get nothing; \
           traffic Windows sends on the user's behalf \
           is not proved blocked: a UNC path (\\\\host\\share) makes the kernel's SMB \
           client connect, the WebClient service carries WebDAV, and ICMP \
           (ping) is sent by the system's own network stack, none of which runs as the \
           sandbox user; two commands that run as the same sandbox user (always two in one \
           folder) can write each other's writable folders and open each other's \
           processes, so a command without the network can read the egress proxy's secret \
           from a proxied command of that user and use the proxy while that command runs; \
           each command's runner holds the owner's logon SID until it protects itself: kGraf \
           writes the lists of the runner's process and main thread while it is suspended and \
           reads them back, but a program of the same sandbox user can still open the runner \
           in the moment between its creation and that write, open a thread the runner starts \
           before it protects itself, or open the whole runner where that write fails; \
           unix-domain sockets are confined only by their own ACLs; a file in a tree \
           kGraf grants that has another hard link keeps the access list it has, because \
           every hard link of a file shares one security descriptor and writing it would \
           change the file under its other name as well: pnpm and uv hard-link package \
           files from a shared store, so a command cannot write such a file in its own \
           tree and a build that writes one in place fails with an access error, while \
           replacing it works; every file and folder that exists when a grant is written \
           carries the entry as an explicit entry of its own, not as the inherited copy \
           Windows would derive, and that entry is the whole tree mask, which the owner \
           chose over a failed run (`icacls` output therefore differs from the token \
           backend's); a file kGraf cannot open or write keeps its old list, as Windows' \
           own propagation also skips a child whose share access is not available, and \
           the command reports each one; a tree granted by an earlier kGraf can hold an \
           entry on a hard-linked file that a removal does not take off, because a \
           removal skips such a file as a grant does; the link check allows a link that \
           leads into a folder the same grant admits the same user to, so a worktree \
           whose dependency link leads to the checkout's installed tree is granted \
           rather than refused: no entry of kGraf's is written through that link, \
           because the pass neither enters a reparse point nor writes one, so what the \
           command reaches there is what the grant on that folder gives it, which for a \
           read root is read; a tree the link check cannot finish \
           (more than 2,000,000 entries, or 60 seconds) refuses the grant, so a command \
           on a tree that large does not run; write confinement, the \
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
            "`dns: confined`",
            "name resolution cache layers",
            "FWP_E_OUT_OF_BOUNDS",
            "shows the filter as FAILED",
            "does not check that the filters are still on the machine",
            "loopback is closed by WFP filters",
            "a listener it started itself included",
            "loopback is not confined on this machine",
            "named object in the session is refused",
            "jobserver",
            "UNC path",
            "WebDAV",
            "ICMP",
            "proxy's secret",
            "runner holds the owner's logon SID",
            "where that write fails",
            "another hard link keeps the access list",
            "explicit entry of its own",
            "cannot open or write keeps its old list",
            "an earlier kGraf",
            "the link check allows a link",
            "cannot finish",
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
/// (`kgraf_core::host_git::LINE_ENDINGS`). Last, for a command that runs as
/// the sandbox user, one `safe.directory` entry per value of `safe`
/// ([`safe_directories`]). None touches `GIT_CONFIG_NOSYSTEM` or
/// `GIT_CONFIG_GLOBAL`, which keep their effect.
pub(super) fn git_config_pairs(network_open: bool, safe: &[String]) -> Vec<(&str, &str)> {
    let mut pairs: Vec<(&str, &str)> = vec![("credential.helper", "")];
    if network_open {
        pairs.push(("http.sslBackend", TLS_BACKEND));
    }
    pairs.extend_from_slice(kgraf_core::host_git::LINE_ENDINGS);
    pairs.extend(safe.iter().map(|v| ("safe.directory", v.as_str())));
    pairs
}

/// The `safe.directory` values a command that runs as the sandbox user gets:
/// each writable root, and every repository below it (`<root>/*`), and each
/// read root, in the form git compares (forward slashes, no `\\?\` prefix).
///
/// Git refuses a repository whose folders another user owns ("detected
/// dubious ownership in repository"), unless `safe.directory` names it in a
/// protected scope; entries given through `GIT_CONFIG_COUNT` are the command
/// scope, which is protected. The worktrees belong to the account kGraf runs as,
/// and the sandbox user is another user, so without these git refuses every
/// command in them: a hosted Windows self-test showed `git status` failing so
/// in the worker's worktree, as the slot user. A repository in a writable
/// root is one the command can rewrite already, its configuration included,
/// so trusting it gives the command nothing it did not have; a read root
/// (the repository's shared git folder) is named alone.
///
/// The token sandbox runs commands as the owner, who owns the folders, so it
/// gets none.
pub(super) fn safe_directories(
    writable: &[std::path::PathBuf],
    readable: &[std::path::PathBuf],
) -> Vec<String> {
    let git_form = |p: &std::path::Path| {
        kgraf_core::plain_path::simplify(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: String| {
        if !out.contains(&v) {
            out.push(v);
        }
    };
    for root in writable {
        let root = git_form(root);
        let root = root.trim_end_matches('/').to_owned();
        push(root.clone());
        push(format!("{root}/*"));
    }
    for dir in readable {
        push(git_form(dir).trim_end_matches('/').to_owned());
    }
    out
}

/// Every variable the sandbox sets for the tools on Windows: the
/// [`git_config_pairs`] as `GIT_CONFIG_*` entries from index zero and their
/// `GIT_CONFIG_COUNT`, and, with the network open, [`CURL_SSL_BACKEND_VAR`].
/// `safe` is [`safe_directories`] for a command that runs as the sandbox
/// user, and empty otherwise.
pub(super) fn tool_env(network_open: bool, safe: &[String]) -> Vec<(String, String)> {
    let mut vars = super::git_config_vars(&git_config_pairs(network_open, safe));
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
    safe: &[String],
) -> Vec<(String, std::ffi::OsString)> {
    let mut kept: Vec<(String, std::ffi::OsString)> = tool_env(network_open, safe)
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

/// Whether the resolved link target `target` lies inside the tree `root` or
/// inside one of `allowed`, the folders the same grant admits the same user
/// to ([`allowed_link_targets`]).
///
/// Paths compare by whole components, case-sensitively: every path given
/// here comes from `canonicalize`, which returns each name as the file
/// system holds it, so two spellings of one path do not reach this. A path
/// that still differs in case counts as outside, which refuses the grant
/// instead of writing it.
pub(super) fn link_target_is_inside(
    target: &std::path::Path,
    root: &std::path::Path,
    allowed: &[std::path::PathBuf],
) -> bool {
    std::iter::once(root)
        .chain(allowed.iter().map(std::path::PathBuf::as_path))
        .any(|inside| target.starts_with(inside))
}

/// The folders a link beneath `grant_on` may lead into without refusing the
/// grant: every folder this same call admits the same user to (`granted`:
/// the writable roots and the read roots), and, for a grant on one of the
/// checkout's own dependency trees (`dependency_trees`), the whole
/// `checkout` that tree belongs to.
///
/// **Why these carry no new reach.** The argument for a link that stays
/// inside the tree is that the propagation would reach only what the grant
/// covers anyway. It holds for `granted` as well: the same user is admitted
/// to those folders in this same call, so an entry carried onto one of them
/// reaches nothing the user could not already reach. `checkout` is narrower
/// than it looks: a grant on a dependency tree is read and run only, and the
/// links inside such a tree are the package manager's own
/// (`node_modules/@scope/pkg` is a junction to a workspace package of the
/// project, outside `node_modules` but inside the checkout). No command can
/// write inside the checkout's dependency trees, so no command can plant a
/// link there.
///
/// The read roots must be in the set, not only the dependency trees: kGraf
/// makes the Gradle wrapper's distribution reachable with a junction inside
/// the command's own tmp folder, which is in a writable root, that leads to
/// the owner's Gradle home or the project's store (`crate::gradle_wrapper`),
/// and both are read roots of the same policy.
///
/// **Still unknown, and why the guard stays.** Whether Windows carries an
/// inheritable entry through a junction at all is not documented, and no
/// Windows run has measured it (`measure_whether_propagation_follows_a_junction`).
/// Where it does, two things are wider than the grant the caller asked for:
/// a write grant on a worktree whose dependency link leads to the checkout's
/// tree would carry **write** onto a folder the user is granted to **read**,
/// and a grant carried onto a folder admits the user to what that folder
/// holds now, not only to what it held when the policy named it. The
/// checkout root is therefore never in the set for a write grant: a command
/// may write in its own worktree and could plant a junction to the checkout
/// root there, and the checkout is the owner's repository.
pub(super) fn allowed_link_targets(
    grant_on: &std::path::Path,
    granted: &[std::path::PathBuf],
    dependency_trees: &[std::path::PathBuf],
    checkout: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut allowed: Vec<std::path::PathBuf> = granted.to_vec();
    if let Some(checkout) = checkout
        && dependency_trees.iter().any(|t| grant_on.starts_with(t))
        && !allowed.iter().any(|a| a == checkout)
    {
        allowed.push(checkout.to_path_buf());
    }
    allowed
}

/// The first link beneath `root` (a junction or symbolic link on Windows,
/// where std's `is_symlink` is true for both) whose target lies neither
/// inside `root` nor inside `allowed` ([`allowed_link_targets`]), or cannot
/// be resolved, with that target; `None` when there is none. Links are never
/// entered.
///
/// Writing an inheritable entry on a folder makes Windows write it on every
/// object that already exists below, and whether that walk enters a junction
/// is not documented. kGraf suppresses that propagation and writes each
/// existing child itself, entering no link ([`child_action`]), so the walk is
/// now the second line of defense and not the first: it holds where the
/// suppression does not, which rests on one documented sentence that no
/// Windows run has measured yet (`imp::open_for_security`). An earlier
/// command could have planted a junction pointing at the owner's files, so a
/// grant for a SID on a tree is refused while such a link exists. A link that leads inside the tree, or into one of the
/// folders the same grant admits the same user to, is allowed: the
/// propagation would reach only what the user may reach anyway (pnpm lays
/// out `node_modules` with in-tree junctions, and kGraf links the checkout's
/// installed dependencies into a worktree). The walk runs only when the
/// entry is absent, and the write that follows it walks the same tree.
/// Bounded in entries and in time ([`WalkLimits`]); a walk that reaches
/// either bound refuses the grant, as a link out of the tree does.
///
/// A command that runs at the same time could plant a link between this walk
/// and the write; the first grant for a SID on a folder happens before any
/// command of that SID runs there, which leaves only commands of another
/// sandbox user in the same folder. The caller walks the tree again after
/// the write for that reason (`apply_inheritable`).
///
/// **A hard link is invisible here, and is handled in the pass instead.**
/// `is_symlink` is true only for a reparse point with the name-surrogate
/// bit, so this walk cannot see a hard link. A refusal would be the wrong
/// answer anyway: in a pnpm or uv tree nearly every file has more than one
/// link, so it would refuse every command of a legitimate project. The
/// propagation pass reads the link count in the open it makes for each child
/// and writes no file that has more than one
/// ([`ChildAction::SkipHardLink`]), which keeps an entry inside the tree
/// without refusing anything.
pub(super) fn first_outside_link(
    root: &std::path::Path,
    allowed: &[std::path::PathBuf],
) -> Result<Option<(std::path::PathBuf, Option<std::path::PathBuf>)>, WalkStop> {
    walk_for_outside_link(root, allowed, &WalkLimits::DEFAULT)
}

/// How far one link walk goes before it gives up ([`first_outside_link`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct WalkLimits {
    /// Directory entries looked at.
    pub entries: usize,
    /// Wall-clock time the whole walk may take.
    pub bound: std::time::Duration,
}

impl WalkLimits {
    /// A worktree with a built `target` folder holds hundreds of thousands
    /// of files and the walk must still finish there, so the entry count is
    /// high: only a tree far larger than any project reaches it. The time
    /// bound is what stops the walk a link to an unreachable network or
    /// removable path would otherwise hold: `canonicalize` on such a link
    /// waits for the operating system's own timeout, which can be tens of
    /// seconds each. Both end in a refusal, never in a grant: a tree that
    /// cannot be checked is not written.
    pub const DEFAULT: Self = Self {
        entries: 2_000_000,
        bound: std::time::Duration::from_secs(60),
    };
}

/// Why a link walk could not finish. Each case names the path or the limit
/// it stopped at, so one unreadable folder deep in a tree is not reported as
/// a failure of the root.
#[derive(Debug)]
pub(super) enum WalkStop {
    /// A folder or an entry in it could not be read.
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// The entry bound was reached.
    TooLarge { entries: usize },
    /// The time bound was reached.
    TimedOut {
        elapsed: std::time::Duration,
        bound: std::time::Duration,
    },
}

impl std::fmt::Display for WalkStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "{} could not be read: {source}", path.display())
            }
            Self::TooLarge { entries } => write!(
                f,
                "the tree holds more than {entries} entries, the most one link check looks at"
            ),
            Self::TimedOut { elapsed, bound } => write!(
                f,
                "the link check ran for {} s, past its bound of {} s; a link to an unreachable \
                 network or removable path can hold it that long",
                elapsed.as_secs(),
                bound.as_secs()
            ),
        }
    }
}

/// [`first_outside_link`] with the limits given, so the limits are tested
/// without a tree of millions of files.
pub(super) fn walk_for_outside_link(
    root: &std::path::Path,
    allowed: &[std::path::PathBuf],
    limits: &WalkLimits,
) -> Result<Option<(std::path::PathBuf, Option<std::path::PathBuf>)>, WalkStop> {
    fn read_error(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> WalkStop + '_ {
        move |source| WalkStop::Read {
            path: path.to_path_buf(),
            source,
        }
    }
    let real_root = kgraf_core::plain_path::canonicalize(root).map_err(read_error(root))?;
    let started = std::time::Instant::now();
    let mut seen = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(read_error(&dir))? {
            seen += 1;
            if seen > limits.entries {
                return Err(WalkStop::TooLarge {
                    entries: limits.entries,
                });
            }
            // Checked per entry: the time a single `canonicalize` of a link
            // to an unreachable path takes cannot be cut short from here, so
            // the walk stops at the next entry after it returns.
            let elapsed = started.elapsed();
            if elapsed > limits.bound {
                return Err(WalkStop::TimedOut {
                    elapsed,
                    bound: limits.bound,
                });
            }
            let entry = entry.map_err(read_error(&dir))?;
            let kind = entry.file_type().map_err(read_error(&entry.path()))?;
            if kind.is_symlink() {
                match kgraf_core::plain_path::canonicalize(entry.path()) {
                    Ok(target) if link_target_is_inside(&target, &real_root, allowed) => {}
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

/// The refusal [`first_outside_link`] leads to. It never asks for the link
/// to be removed: the links kGraf makes in a worktree are kGraf's own, and
/// it would make them again.
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
        "{} holds the link {} to {target}, which this command is not granted. Writing the \
         sandbox's access entry on {} could carry it through the link onto {target}, so nothing \
         was written. The dependency trees kGraf links into a tree are granted, and a link \
         inside the tree is allowed, so this link leads somewhere else: it has to lead inside \
         the tree, or into a folder the command is granted, before a command can run here.",
        root.display(),
        link.display(),
        root.display()
    )
}

/// The note a command carries for a read root the link walk refused. The
/// root is not granted, so the command can run: a read it is not admitted
/// to takes nothing away from the sandbox, and a failure instead would stop
/// every command on a machine whose toolchain holds a link of the owner's
/// own (`rustup toolchain link`, a link in a cargo registry crate). The
/// note goes out with the command's output
/// (`super::SandboxedCommand::refused_reads`).
pub(super) fn refused_read_note(dir: &std::path::Path, why: &str) -> String {
    format!(
        "the sandboxed command is not granted {}, so a read there fails: {why}",
        dir.display()
    )
}

/// How a guarded write of an inheritable entry failed
/// ([`guarded_inheritable_write`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GuardedWrite<E> {
    /// The walk refused the tree before the write: nothing was written.
    Refused(String),
    /// The write itself failed.
    Write(E),
    /// The entry was written, and the walk then found a link the walk
    /// before the write did not: the entry may already have been carried
    /// onto that link's target.
    PlantedAfterWrite(String),
}

/// Write an inheritable entry on a tree, with the link walk before and
/// after the write: `walk` reports why the tree must not be written
/// ([`first_outside_link`]), `write` writes the entry on the folder and then
/// on each existing child itself ([`child_action`]).
///
/// Why the walk runs twice. kGraf writes the tree's entries in one pass of
/// its own, and a command of another sandbox user that may write in the same
/// folder can plant a junction between the walk and that pass (`mklink /J`
/// needs no privilege). The window cannot be closed from here, so it is
/// reported instead of passing unseen: a link the second walk finds means the
/// pass may already have reached the link's target. The caller then fails
/// loud, with the path.
///
/// The sequencing is here, not in the backend, so it is tested on any host.
pub(super) fn guarded_inheritable_write<T, E>(
    walk: impl Fn() -> Option<String>,
    write: impl FnOnce() -> Result<T, E>,
) -> Result<T, GuardedWrite<E>> {
    if let Some(why) = walk() {
        return Err(GuardedWrite::Refused(why));
    }
    let out = write().map_err(GuardedWrite::Write)?;
    match walk() {
        None => Ok(out),
        Some(why) => Err(GuardedWrite::PlantedAfterWrite(why)),
    }
}

/// What kGraf's own propagation pass does with one existing child of a tree
/// whose access list it just wrote.
///
/// Windows propagates an inheritable entry to every existing child by
/// itself, and it walks the tree for that with rules kGraf cannot steer: a
/// junction may be entered, and a hard link carries the entry onto a file
/// that is also named outside the tree, because every hard link of a file
/// shares one security descriptor. kGraf therefore suppresses that
/// propagation (`imp::open_for_security`) and makes the pass itself, one
/// child at a time, so the two rules below hold.
///
/// **The tree boundary is what this pass protects.** A reparse point is not
/// entered and not written, and a file with more than one hard link is never
/// written. Those two rules, and nothing else here, keep an entry inside the
/// tree it was asked for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChildAction {
    /// A folder: write the entry on it, with the same inheritable flags the
    /// root carries, so everything made in it later gets it from Windows,
    /// and enter it.
    WriteFolder,
    /// A file: write the effective entry on it. A file inherits nothing
    /// further, so the entry carries no inheritance flags.
    WriteFile,
    /// A reparse point (a junction or a symbolic link; both carry the
    /// name-surrogate bit): never entered, never written. The walk in front
    /// of the write already refused a link that leads out of the tree
    /// ([`first_outside_link`]), and the walk behind it reports one planted
    /// meanwhile, so the pass itself only has to leave it alone. A link that
    /// leads inside the tree needs nothing: the pass reaches its target by
    /// the target's own name.
    SkipLink,
    /// A file with more than one hard link: never written, and recorded.
    ///
    /// A file's security descriptor belongs to the file, not to the
    /// directory entry, so every hard link of it shares one descriptor.
    /// pnpm and uv hard-link package files from a shared store into a
    /// project, so writing such a file's descriptor would write the owner's
    /// store file as well, wherever it is (decision 0083). The file keeps the
    /// list it has, which is the right outcome for a store file: the command
    /// needs to read it, and the read grant on the store is what makes the
    /// dependency usable.
    ///
    /// **A command therefore cannot write a hard-linked file in its own
    /// tree.** A package manager that writes such a file in place, instead of
    /// replacing it, fails with an access error; replacing it works, because
    /// the new file is a new object and inherits the folder's entry. The
    /// same holds for a hard link made after the first grant: it carries the
    /// file's own descriptor, not the folder's inheritance.
    SkipHardLink,
    /// A folder kGraf denies this user (the top-level `.git`, a hidden
    /// folder): not written and not entered, so a generous entry never lands
    /// where a deny is meant to hold.
    SkipDenied,
}

/// What kGraf's propagation pass does with one existing child
/// ([`ChildAction`]). `links` is `nNumberOfLinks` of the child, read in the
/// same open as `is_reparse` and `is_dir`, so no second open is needed.
///
/// The order of the rules is the order of the risks: what kGraf denies, then
/// a link, then a hard-linked file, and only then a write. `denied` holds
/// whole folders, so a path inside one is skipped as well, which matters
/// where the caller named a folder it had already entered.
pub(super) fn child_action(
    path: &std::path::Path,
    is_reparse: bool,
    is_dir: bool,
    links: u32,
    denied: &[std::path::PathBuf],
) -> ChildAction {
    if denied.iter().any(|d| path.starts_with(d)) {
        return ChildAction::SkipDenied;
    }
    if is_reparse {
        return ChildAction::SkipLink;
    }
    if is_dir {
        // NTFS keeps no hard link to a folder, so the link count of a folder
        // is not checked: a folder is entered and written.
        return ChildAction::WriteFolder;
    }
    if links > 1 {
        return ChildAction::SkipHardLink;
    }
    ChildAction::WriteFile
}

/// What one propagation pass wrote and what it left alone
/// ([`child_action`]). The counts and the paths go into the note the command
/// carries, so a child that did not get the entry is never silent.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Skipped {
    /// Children whose list was written.
    pub written: usize,
    /// Reparse points, not entered and not written.
    pub links: usize,
    /// Folders kGraf denies, not entered and not written.
    pub denied: usize,
    /// How many files with more than one hard link were not written
    /// ([`ChildAction::SkipHardLink`]), and the first [`NOTE_PATHS`] of
    /// them. Only the first few are kept: a pnpm or uv tree can hold a
    /// hundred thousand of them, and a list of every path would cost more
    /// memory than the pass itself.
    pub hard_links: usize,
    pub first_hard_links: Vec<std::path::PathBuf>,
    /// Children kGraf could not open, read or write, with the reason. The
    /// pass goes on, as Windows' own propagation does: Microsoft's
    /// `SetSecurityInfo` page states that "if share access to the children
    /// of the object is not available, this function will not propagate
    /// unprotected ACEs to the children", so a file another program holds
    /// open without sharing is skipped by Windows too, and failing here
    /// would refuse a grant that used to work.
    pub blocked: usize,
    /// The first [`NOTE_PATHS`] of them, with the reason.
    pub first_blocked: Vec<(std::path::PathBuf, String)>,
}

/// How many paths one note lists before it only counts them, so a tree with
/// thousands of hard links does not fill a command's output.
const NOTE_PATHS: usize = 5;

impl Skipped {
    /// Record a file that was not written because it has another hard link.
    fn hard_link(&mut self, path: std::path::PathBuf) {
        self.hard_links += 1;
        if self.first_hard_links.len() < NOTE_PATHS {
            self.first_hard_links.push(path);
        }
    }

    /// Record a child that could not be opened, written or listed.
    fn blocked(&mut self, path: std::path::PathBuf, why: String) {
        self.blocked += 1;
        if self.first_blocked.len() < NOTE_PATHS {
            self.first_blocked.push((path, why));
        }
    }

    /// The note a command carries for the children this pass left alone, or
    /// `None` when it left none. `what` names the write (for example "the
    /// write grant"), and `root` the tree.
    pub(super) fn note(&self, root: &std::path::Path, what: &str) -> Option<String> {
        if self.hard_links == 0 && self.blocked == 0 {
            return None;
        }
        let mut note = format!("{what} on {} reached {} ", root.display(), self.written);
        note.push_str("of its files and folders; these keep the access list they had");
        if self.hard_links > 0 {
            note.push_str(&format!(
                ", {} because another hard link of the same file lies outside this tree and \
                 would be changed with it ({})",
                self.hard_links,
                names(&self.first_hard_links, self.hard_links)
            ));
        }
        if self.blocked > 0 {
            note.push_str(&format!(
                ", {} because they could not be opened or written ({})",
                self.blocked,
                reasons(&self.first_blocked, self.blocked)
            ));
        }
        note.push_str(
            ". A command cannot write them, and a build that writes one in place \
                       fails with an access error.",
        );
        Some(note)
    }
}

/// What the one open per child reports to kGraf's propagation pass
/// ([`propagation_plan`]).
pub(super) struct ChildFacts<H> {
    /// A junction or a symbolic link: both carry the name-surrogate bit.
    pub is_reparse: bool,
    /// A folder.
    pub is_dir: bool,
    /// `nNumberOfLinks`: how many names this file has on its volume.
    pub links: u32,
    /// What the write goes through, so the child is opened once.
    pub handle: H,
}

/// kGraf's propagation pass over the existing children of `root`, with the
/// three operations it needs injected, so the walk itself is tested on any
/// host ([`imp::propagate`] is the Windows adapter).
///
/// `list` names the children of one folder. `probe` makes the one open per
/// child and reports [`ChildFacts`] from it: the reparse state, the kind,
/// the hard-link count and the handle to write through. `write` writes the
/// entry through that handle.
///
/// The rules the walk holds, and the reason each is here:
/// - a reparse point is never entered and never written, so nothing of ours
///   reaches what it points at;
/// - a folder kGraf denies is never entered and never written, so a
///   generous entry never lands where a deny must hold;
/// - a file with more than one hard link is never written, so nothing of
///   ours reaches the file under a name outside the tree;
/// - a child that cannot be opened, written or listed is recorded and the
///   walk goes on, as Windows' own propagation does
///   ([`Skipped::blocked`]).
///
/// Only a failure to list `root` itself is an error: the folder's own list
/// is written by then, so the pass must reach what it can.
pub(super) fn propagation_plan<H>(
    root: &std::path::Path,
    denied: &[std::path::PathBuf],
    list: &mut dyn FnMut(&std::path::Path) -> Result<Vec<std::path::PathBuf>, String>,
    probe: &mut dyn FnMut(&std::path::Path) -> Result<ChildFacts<H>, String>,
    write: &mut dyn FnMut(&H, &std::path::Path, bool) -> Result<(), String>,
) -> Result<Skipped, String> {
    let mut skipped = Skipped::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let children = match list(&dir) {
            Ok(children) => children,
            Err(why) if dir == root => return Err(why),
            Err(why) => {
                skipped.blocked(dir, why);
                continue;
            }
        };
        for path in children {
            let facts = match probe(&path) {
                Ok(facts) => facts,
                Err(why) => {
                    skipped.blocked(path, why);
                    continue;
                }
            };
            match child_action(&path, facts.is_reparse, facts.is_dir, facts.links, denied) {
                ChildAction::WriteFolder => {
                    if let Err(why) = write(&facts.handle, &path, true) {
                        skipped.blocked(path.clone(), why);
                    } else {
                        skipped.written += 1;
                    }
                    // Entered even when its own write failed: a child of it
                    // may still take the entry.
                    pending.push(path);
                }
                ChildAction::WriteFile => {
                    if let Err(why) = write(&facts.handle, &path, false) {
                        skipped.blocked(path, why);
                    } else {
                        skipped.written += 1;
                    }
                }
                ChildAction::SkipLink => skipped.links += 1,
                ChildAction::SkipDenied => skipped.denied += 1,
                ChildAction::SkipHardLink => skipped.hard_link(path),
            }
        }
    }
    Ok(skipped)
}

/// The paths kept, with a count of the rest (`total` is how many there
/// were).
fn names(paths: &[std::path::PathBuf], total: usize) -> String {
    let shown: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let mut out = shown.join(", ");
    if total > paths.len() {
        out.push_str(&format!(" and {} more", total - paths.len()));
    }
    out
}

/// The paths kept with their reasons, with a count of the rest.
fn reasons(paths: &[(std::path::PathBuf, String)], total: usize) -> String {
    let shown: Vec<String> = paths
        .iter()
        .map(|(p, why)| format!("{}: {why}", p.display()))
        .collect();
    let mut out = shown.join("; ");
    if total > paths.len() {
        out.push_str(&format!("; and {} more", total - paths.len()));
    }
    out
}

/// How one write of an object's access list in a granted tree is kept off
/// the objects below it. Chosen by how the object's handle could be opened
/// ([`lone_write_after`]); every grant, deny and removal pass writes through
/// one of the two, and kGraf's own pass ([`propagation_plan`]) stays the only
/// writer of the children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListWrite {
    /// The handle was opened with `MAXIMUM_ALLOWED`, and `SetSecurityInfo`
    /// writes the list. Microsoft's `SetSecurityInfo` page: "If the supplied
    /// handle was opened with an ACCESS_MASK value of MAXIMUM_ALLOWED, then
    /// the SetSecurityInfo function will not propagate ACEs to children."
    /// The same call still takes the parent's inheritable entries into this
    /// object's own list when that list is not protected (measured on
    /// Windows, decision 0097), so a removal that writes a child this way
    /// also drops the inherited copies of an entry its parent lost.
    SetSecurityInfo,
    /// The `MAXIMUM_ALLOWED` open met `ERROR_SHARING_VIOLATION`: a handle on
    /// the object is open without `FILE_SHARE_DELETE` (Windows and its
    /// services held `C:\` and `C:\Windows\Tasks` that way; an editor, an
    /// indexer or a watcher can do the same in a worktree), and
    /// `MAXIMUM_ALLOWED` on an object kGraf controls is given `DELETE`, which
    /// takes part in sharing checks. A process whose current folder is the
    /// folder does not cause it: on Windows Server 2025 a folder that `ping`
    /// had as its current folder still opened with `MAXIMUM_ALLOWED`. The second handle holds only
    /// `READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES`, none of which does.
    /// `SetSecurityInfo` would propagate through such a handle (the same
    /// page: "the system automatically propagates any inheritable access
    /// control entries (ACEs) to existing child objects"), so the list is
    /// written with `SetKernelObjectSecurity`, read back and rolled back on
    /// any difference (`secure::write_dacl_alone`). Microsoft's page on
    /// automatic propagation names `SetNamedSecurityInfo` and
    /// `SetSecurityInfo` as the functions that propagate, and the
    /// `SetKernelObjectSecurity` page says it "should not be used when
    /// setting a security descriptor on file system objects" for that
    /// reason; nothing propagating is what this write needs. That this call
    /// changes no child is measured on Windows, not documented in so many
    /// words. It also takes nothing in from the parent, so a removal does
    /// that part itself ([`removal_keeps`]).
    KernelObjectAlone,
}

/// `ERROR_SHARING_VIOLATION`.
const ERROR_SHARING_VIOLATION: u32 = 32;
/// `ERROR_ACCESS_DENIED`.
const ERROR_ACCESS_DENIED_CODE: u32 = 5;

/// Whether a `MAXIMUM_ALLOWED` open that failed with the Win32 error `code`
/// is opened a second time for a lone write ([`ListWrite::KernelObjectAlone`]).
///
/// A sharing violation is: a smaller mask answers exactly that refusal. A
/// refusal is too. A hosted Windows self-test refused the `MAXIMUM_ALLOWED`
/// open of git's loose objects in the owner's own repository with error 5
/// ("the read grant on ...\.git reached 84 of its files and folders; ... 14
/// because they could not be opened or written", each an object file),
/// where the owner holds every right: git writes those files read-only, and
/// a `MAXIMUM_ALLOWED` open of a read-only file is refused although the
/// list grants the caller everything (inferred from which files failed; not
/// documented). The second open asks only for `READ_CONTROL`, `WRITE_DAC`
/// and `FILE_READ_ATTRIBUTES`, none of which the read-only attribute
/// refuses; where the caller truly has no right, it is refused the same way
/// and the refusal stands. Every other error stands, as it did before this
/// second open existed: a missing object and a bad path would fail the
/// second open the same way.
pub(super) fn lone_write_after(code: u32) -> bool {
    code == ERROR_SHARING_VIOLATION || code == ERROR_ACCESS_DENIED_CODE
}

/// One entry of a child's access list, as a removal judges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EntryFacts {
    /// An allow entry (a deny only narrows, and stays).
    pub allow: bool,
    /// Marked `INHERITED_ACE`: a copy of an entry of a folder above.
    pub inherited: bool,
    /// It names one of the SIDs being removed.
    pub names_removed: bool,
}

/// Whether a removal writes a child whose list holds `entries`, through a
/// handle that writes as `write` does.
///
/// A child with an explicit entry for a removed SID is written either way;
/// that is what the pass is for. Through [`ListWrite::KernelObjectAlone`] a
/// child is also written for an inherited allow entry for a removed SID:
/// `SetSecurityInfo` would have dropped that copy as it took in the parent's
/// list again, and the lone write takes nothing in, so it must drop the copy
/// itself ([`removal_keeps`]), or the slot user would keep its write access
/// to a folder that another program held open.
///
/// Through [`ListWrite::SetSecurityInfo`] a child that holds only inherited
/// entries is still skipped, as before: whether its inherited copy goes when
/// the folder's entry goes is an open measurement (decision 0097), and
/// writing every such child would rewrite every file a build made.
pub(super) fn removal_writes(write: ListWrite, entries: &[EntryFacts]) -> bool {
    entries.iter().any(|e| {
        e.names_removed && (!e.inherited || (write == ListWrite::KernelObjectAlone && e.allow))
    })
}

/// Whether an entry stays on a child after a removal's merge, when the list
/// is written as `write` does. The merge (`SetEntriesInAclW` with
/// `REVOKE_ACCESS`) has taken the explicit entries off already; this decides
/// the inherited ones.
///
/// Through [`ListWrite::KernelObjectAlone`] an inherited allow entry for a
/// removed SID is dropped. The pass writes a folder before it enters it, so
/// when a child is written its parent no longer holds the entry this copy
/// came from, and Windows' own rule is that a copy goes with its source:
/// "If you remove an inheritable ACE from a parent object, automatic
/// inheritance removes any copies of the ACE that were inherited by child
/// objects" (Microsoft, Automatic Propagation of Inheritable ACEs). A copy
/// whose source lies above the removed tree is dropped too; that only
/// narrows the removed user's access, as the merge already does for that
/// user's explicit entries from such a source. Every other entry stays.
pub(super) fn removal_keeps(write: ListWrite, entry: EntryFacts) -> bool {
    !(write == ListWrite::KernelObjectAlone
        && entry.inherited
        && entry.allow
        && entry.names_removed)
}

/// The two ways a list write stays on one object, and what a removal does
/// on a child through each. These run on every host.
#[cfg(test)]
mod list_write_tests {
    use super::*;

    const fn e(allow: bool, inherited: bool, names_removed: bool) -> EntryFacts {
        EntryFacts {
            allow,
            inherited,
            names_removed,
        }
    }

    /// A sharing violation and a refusal open the object again for a lone
    /// write; nothing else does. The refusal is the read-only loose objects
    /// of git that a hosted Windows self-test's read grant on a repository's `.git` could not
    /// open with `MAXIMUM_ALLOWED`.
    #[test]
    fn a_sharing_violation_or_a_refusal_opens_the_object_again_for_a_lone_write() {
        assert!(lone_write_after(32));
        assert!(lone_write_after(5));
        // File and path not found, a bad name, a lock, no error.
        for code in [2, 3, 123, 33, 0] {
            assert!(!lone_write_after(code), "error {code}");
        }
    }

    #[test]
    fn an_explicit_entry_for_a_removed_user_is_written_whatever_the_handle() {
        for write in [ListWrite::SetSecurityInfo, ListWrite::KernelObjectAlone] {
            assert!(removal_writes(write, &[e(true, false, true)]), "{write:?}");
            assert!(removal_writes(write, &[e(false, false, true)]), "{write:?}");
            // Nothing naming a removed user: never written, so a pass over a
            // built tree does not rewrite what has nothing to do with it.
            assert!(
                !removal_writes(write, &[e(true, false, false), e(true, true, false)]),
                "{write:?}"
            );
            assert!(!removal_writes(write, &[]), "{write:?}");
        }
    }

    #[test]
    fn an_inherited_allow_for_a_removed_user_is_written_only_by_a_lone_write() {
        let only_inherited = [e(true, true, true), e(true, false, false)];
        assert!(removal_writes(
            ListWrite::KernelObjectAlone,
            &only_inherited
        ));
        assert!(!removal_writes(ListWrite::SetSecurityInfo, &only_inherited));
        // An inherited deny only narrows: no write for it alone.
        assert!(!removal_writes(
            ListWrite::KernelObjectAlone,
            &[e(false, true, true)]
        ));
    }

    #[test]
    fn a_lone_write_drops_the_inherited_allow_copy_and_keeps_every_other_entry() {
        let alone = ListWrite::KernelObjectAlone;
        assert!(!removal_keeps(alone, e(true, true, true)));
        for kept in [
            e(false, true, true),
            e(true, true, false),
            e(true, false, false),
            e(false, false, false),
            // An explicit entry is the merge's to take off, not this rule's.
            e(true, false, true),
        ] {
            assert!(removal_keeps(alone, kept), "{kept:?}");
        }
    }

    #[test]
    fn a_write_through_set_security_info_keeps_what_the_merge_left() {
        for allow in [false, true] {
            for inherited in [false, true] {
                for names_removed in [false, true] {
                    let entry = e(allow, inherited, names_removed);
                    assert!(
                        removal_keeps(ListWrite::SetSecurityInfo, entry),
                        "{entry:?}"
                    );
                }
            }
        }
    }
}

/// The per-child decision of the propagation pass, and the note it leaves.
/// These run on every host: the decision is data, not a Windows call.
#[cfg(test)]
mod child_action_tests {
    use std::path::{Path, PathBuf};

    use super::{ChildAction, ChildFacts, Skipped, child_action};

    #[test]
    fn a_file_with_one_link_is_written_and_a_folder_carries_the_inheritance() {
        assert_eq!(
            child_action(Path::new("t/a.rs"), false, false, 1, &[]),
            ChildAction::WriteFile
        );
        assert_eq!(
            child_action(Path::new("t/src"), false, true, 1, &[]),
            ChildAction::WriteFolder
        );
    }

    /// A file that existed before the grant carries the whole entry set the
    /// root carries, the `OWNER RIGHTS` restriction and kGraf's compensating
    /// entry included, and neither of those reaches a hard-linked file or
    /// anything through a link.
    ///
    /// Why this test exists. The owner-rights restriction keeps a sandbox
    /// user that owns a file it created from rewriting that file's own list
    /// after a revoke took its entries away. It used to reach the files that
    /// already existed through the kernel's propagation of the root's
    /// inheritable entries; that propagation is suppressed now
    /// ([`imp::open_for_security`]), so the set must be written per child, by
    /// the pass, or the restriction would hold only for files created after
    /// the grant.
    #[test]
    fn a_file_that_existed_before_the_grant_carries_the_owner_rights_restriction() {
        use crate::sandbox_user::acl::{
            READ_CONTROL_RIGHT, Scope, WRITE_DAC_RIGHT, Who, child_grant, tree_grant,
        };
        const ROOT_MASK: u32 = 0x0012_01bf;
        const TREE_MASK: u32 = ROOT_MASK | 0x0001_0000;
        let aces = tree_grant(ROOT_MASK, TREE_MASK);

        // A file that was in the worktree before the grant is a child the
        // pass writes.
        let old = Path::new("t/src/main.rs");
        assert_eq!(
            child_action(old, false, false, 1, &[]),
            ChildAction::WriteFile
        );
        let on_file = child_grant(&aces, false);
        let mask_of = |who: Who| -> Vec<u32> {
            on_file
                .iter()
                .filter(|a| a.who == who)
                .map(|a| a.mask)
                .collect()
        };
        assert_eq!(mask_of(Who::SandboxUser), vec![TREE_MASK], "the user");
        assert_eq!(
            mask_of(Who::OwnerRights),
            vec![READ_CONTROL_RIGHT],
            "an owner of this file may read its list and change nothing"
        );
        assert_eq!(
            mask_of(Who::Owner),
            vec![READ_CONTROL_RIGHT | WRITE_DAC_RIGHT],
            "kGraf keeps the two rights the entry above would take from it"
        );
        // Nothing on a file is inheritable: a file has nothing below it.
        assert!(on_file.iter().all(|a| a.scope == Scope::Folder));

        // A folder that existed before the grant carries the same three
        // trustees, effective on it and inherited by what is made in it.
        let on_dir = child_grant(&aces, true);
        for who in [Who::SandboxUser, Who::OwnerRights, Who::Owner] {
            assert!(
                on_dir
                    .iter()
                    .any(|a| a.who == who && a.scope == Scope::HereAndBelow),
                "{who:?} is missing on a folder of the tree"
            );
        }
        assert_eq!(on_dir.len(), on_file.len(), "the same trustees either way");

        // The root keeps its own set, which is where these came from: one
        // derivation, used at the root and per child.
        assert_eq!(aces.len(), 6);

        // And neither entry is ever written on a hard-linked file or
        // through a link: the pass does not write those children at all.
        assert_eq!(
            child_action(
                Path::new("t/node_modules/.pnpm/x/index.js"),
                false,
                false,
                2,
                &[]
            ),
            ChildAction::SkipHardLink
        );
        assert_eq!(
            child_action(Path::new("t/link"), true, true, 1, &[]),
            ChildAction::SkipLink
        );
        assert_eq!(
            child_action(Path::new("t/link.txt"), true, false, 1, &[]),
            ChildAction::SkipLink
        );
    }

    /// The gap this pass closes: a file with a second hard link shares one
    /// security descriptor with it, so its list is never written. pnpm and
    /// uv hard-link package files from a shared store into a project.
    #[test]
    fn a_hard_linked_file_is_never_written() {
        assert_eq!(
            child_action(
                Path::new("t/node_modules/.pnpm/x/index.js"),
                false,
                false,
                2,
                &[]
            ),
            ChildAction::SkipHardLink
        );
        assert_eq!(
            child_action(Path::new("t/big.bin"), false, false, 9, &[]),
            ChildAction::SkipHardLink
        );
    }

    /// A folder's link count is not a hard link: NTFS keeps none to a
    /// folder, and a folder must be written and entered.
    #[test]
    fn a_folder_is_written_whatever_its_link_count_says() {
        assert_eq!(
            child_action(Path::new("t/src"), false, true, 3, &[]),
            ChildAction::WriteFolder
        );
    }

    /// A junction or symbolic link is left alone, whatever it points at:
    /// the walk in front of the write judges the target, the pass does not
    /// enter it, so no entry of ours can reach what it points at.
    #[test]
    fn a_link_is_left_alone_and_a_denied_folder_is_skipped_first() {
        assert_eq!(
            child_action(Path::new("t/node_modules"), true, true, 1, &[]),
            ChildAction::SkipLink
        );
        assert_eq!(
            child_action(Path::new("t/out"), true, true, 1, &[]),
            ChildAction::SkipLink
        );
        let denied = vec![PathBuf::from("t/.git"), PathBuf::from("t/secrets")];
        // A denied folder, a file inside it, and a link inside it: none is
        // written, so a generous entry never lands where a deny must hold.
        assert_eq!(
            child_action(Path::new("t/.git"), false, true, 1, &denied),
            ChildAction::SkipDenied
        );
        assert_eq!(
            child_action(Path::new("t/.git/config"), false, false, 1, &denied),
            ChildAction::SkipDenied
        );
        assert_eq!(
            child_action(Path::new("t/secrets/k"), true, true, 1, &denied),
            ChildAction::SkipDenied
        );
        // A name that only starts with the same letters is not denied.
        assert_eq!(
            child_action(Path::new("t/.gitignore"), false, false, 1, &denied),
            ChildAction::WriteFile
        );
    }

    /// One tree, described as the pass would see it: `(path, reparse, dir,
    /// links)`. The listing and the probe come from this, so the walk is
    /// tested without a file system.
    const TREE: &[(&str, bool, bool, u32)] = &[
        ("t/src", false, true, 1),
        ("t/src/main.rs", false, false, 1),
        ("t/src/store.js", false, false, 3),
        ("t/.git", false, true, 1),
        ("t/.git/config", false, false, 1),
        ("t/jn", true, true, 1),
        ("t/jn/outside.txt", false, false, 1),
        ("t/README.md", false, false, 1),
    ];

    /// `list`, `probe` and `write` over [`TREE`], and what `write` reached.
    fn walk(denied: &[PathBuf]) -> (Skipped, Vec<String>) {
        let mut written: Vec<String> = Vec::new();
        let skipped = {
            let mut list = |dir: &Path| -> Result<Vec<PathBuf>, String> {
                Ok(TREE
                    .iter()
                    .map(|(p, ..)| PathBuf::from(p))
                    .filter(|p| p.parent() == Some(dir))
                    .collect())
            };
            let mut probe = |path: &Path| -> Result<ChildFacts<()>, String> {
                TREE.iter()
                    .find(|(p, ..)| Path::new(p) == path)
                    .map(|&(_, is_reparse, is_dir, links)| ChildFacts {
                        is_reparse,
                        is_dir,
                        links,
                        handle: (),
                    })
                    .ok_or_else(|| "no such child".to_owned())
            };
            let mut write = |_: &(), path: &Path, _: bool| -> Result<(), String> {
                written.push(path.display().to_string());
                Ok(())
            };
            super::propagation_plan(Path::new("t"), denied, &mut list, &mut probe, &mut write)
                .expect("the root is listed")
        };
        written.sort();
        (skipped, written)
    }

    /// Gap 1 of the review, and the rule that holds the tree boundary: the
    /// pass writes every plain child, enters no link, and writes no file
    /// that has a second hard link.
    #[test]
    fn the_pass_enters_no_link_and_writes_no_hard_linked_file() {
        let (skipped, written) = walk(&[]);
        assert_eq!(
            written,
            vec![
                "t/.git",
                "t/.git/config",
                "t/README.md",
                "t/src",
                "t/src/main.rs",
            ]
        );
        assert_eq!(
            (skipped.hard_links, skipped.first_hard_links.as_slice()),
            (1, [PathBuf::from("t/src/store.js")].as_slice())
        );
        assert_eq!((skipped.links, skipped.denied, skipped.written), (1, 0, 5));
        // Nothing below the junction was reached, by any name.
        assert!(
            !written.iter().any(|p| p.contains("outside")),
            "{written:?}"
        );
    }

    /// A grant holds back what kGraf denies; a removal passes an empty set,
    /// so it reaches every child and can take an entry off inside `.git`
    /// as well. That difference is gap 2 of the review.
    #[test]
    fn a_denied_folder_is_not_entered_and_a_removal_enters_everything() {
        let (skipped, written) = walk(&[PathBuf::from("t/.git")]);
        assert_eq!(written, vec!["t/README.md", "t/src", "t/src/main.rs"]);
        assert_eq!(skipped.denied, 1, "the denied folder was counted once");
        assert!(!written.iter().any(|p| p.contains(".git")), "{written:?}");
        // The removal pass names nothing as denied.
        let (_, all) = walk(&[]);
        assert!(all.contains(&"t/.git/config".to_owned()), "{all:?}");
    }

    /// A child the pass cannot open, or a folder it cannot list, is
    /// recorded and the walk goes on. Windows' own propagation skips such a
    /// child as well, so failing here would refuse a grant that worked.
    #[test]
    fn a_child_that_cannot_be_opened_is_recorded_and_the_walk_goes_on() {
        let mut list = |dir: &Path| -> Result<Vec<PathBuf>, String> {
            match dir.file_name().and_then(|n| n.to_str()) {
                Some("t") => Ok(vec![
                    PathBuf::from("t/locked.db"),
                    PathBuf::from("t/closed"),
                    PathBuf::from("t/ok.rs"),
                ]),
                Some("closed") => Err("access is denied".to_owned()),
                _ => Ok(Vec::new()),
            }
        };
        let facts = |is_dir: bool| ChildFacts {
            is_reparse: false,
            is_dir,
            links: 1,
            handle: (),
        };
        let mut probe = |path: &Path| -> Result<ChildFacts<()>, String> {
            match path.file_name().and_then(|n| n.to_str()) {
                Some("locked.db") => Err("sharing violation".to_owned()),
                Some("closed") => Ok(facts(true)),
                _ => Ok(facts(false)),
            }
        };
        let mut written: Vec<String> = Vec::new();
        let mut write = |_: &(), path: &Path, _: bool| -> Result<(), String> {
            written.push(path.display().to_string());
            Ok(())
        };
        let skipped =
            super::propagation_plan(Path::new("t"), &[], &mut list, &mut probe, &mut write)
                .expect("the root is listed");
        written.sort();
        assert_eq!(written, vec!["t/closed", "t/ok.rs"]);
        assert_eq!(skipped.blocked, 2);
        assert_eq!(
            skipped.first_blocked,
            vec![
                (PathBuf::from("t/locked.db"), "sharing violation".to_owned()),
                (PathBuf::from("t/closed"), "access is denied".to_owned()),
            ]
        );
    }

    /// A root that cannot be listed is the one failure of the pass: the
    /// folder's own list is written by then, so the caller must hear it.
    #[test]
    fn a_root_that_cannot_be_listed_is_an_error() {
        let mut list = |_: &Path| -> Result<Vec<PathBuf>, String> { Err("gone".to_owned()) };
        let mut probe = |_: &Path| -> Result<ChildFacts<()>, String> { unreachable!() };
        let mut write = |_: &(), _: &Path, _: bool| -> Result<(), String> { unreachable!() };
        assert_eq!(
            super::propagation_plan(Path::new("t"), &[], &mut list, &mut probe, &mut write,),
            Err("gone".to_owned())
        );
    }

    #[test]
    fn the_note_names_the_hard_links_and_counts_the_rest() {
        let mut skipped = Skipped {
            written: 12,
            links: 2,
            denied: 1,
            ..Skipped::default()
        };
        assert_eq!(skipped.note(Path::new("t"), "the write grant"), None);
        for i in 0..7 {
            skipped.hard_link(PathBuf::from(format!("t/f{i}.js")));
        }
        skipped.blocked(PathBuf::from("t/locked.db"), "sharing violation".to_owned());
        let note = skipped
            .note(Path::new("t"), "the write grant")
            .expect("a note");
        assert!(note.contains("the write grant on t reached 12"), "{note}");
        assert!(note.contains("t/f0.js"), "{note}");
        assert!(note.contains("and 2 more"), "{note}");
        assert!(!note.contains("t/f6.js"), "{note}");
        assert!(note.contains("t/locked.db: sharing violation"), "{note}");
        assert!(note.contains("fails with an access error"), "{note}");
    }
}

#[cfg(test)]
mod guarded_write_tests {
    use std::cell::Cell;

    use super::{GuardedWrite, guarded_inheritable_write};

    #[test]
    fn a_refused_tree_is_never_written() {
        let writes = Cell::new(0);
        let out = guarded_inheritable_write::<(), &str>(
            || Some("a link leads out".to_owned()),
            || {
                writes.set(writes.get() + 1);
                Ok(())
            },
        );
        assert_eq!(
            out,
            Err(GuardedWrite::Refused("a link leads out".to_owned()))
        );
        assert_eq!(writes.get(), 0, "the entry must not be written");
    }

    #[test]
    fn a_clean_tree_is_written_once_and_walked_twice() {
        let walks = Cell::new(0);
        let writes = Cell::new(0);
        let out = guarded_inheritable_write::<(), &str>(
            || {
                walks.set(walks.get() + 1);
                None
            },
            || {
                writes.set(writes.get() + 1);
                Ok(())
            },
        );
        assert_eq!(out, Ok(()));
        assert_eq!((walks.get(), writes.get()), (2, 1));
    }

    /// A link planted between the walk and the write is reported, not
    /// passed over: the entry is already written then.
    #[test]
    fn a_link_planted_after_the_walk_is_reported() {
        let walks = Cell::new(0);
        let out = guarded_inheritable_write::<(), &str>(
            || {
                walks.set(walks.get() + 1);
                (walks.get() > 1).then(|| "a junction appeared".to_owned())
            },
            || Ok(()),
        );
        assert_eq!(
            out,
            Err(GuardedWrite::PlantedAfterWrite(
                "a junction appeared".to_owned()
            ))
        );
    }

    #[test]
    fn a_failed_write_keeps_its_own_error() {
        let out = guarded_inheritable_write(|| None, || Err::<(), _>("SetSecurityInfo failed"));
        assert_eq!(out, Err(GuardedWrite::Write("SetSecurityInfo failed")));
    }
}

#[cfg(all(test, unix))]
mod outside_link_tests {
    use std::path::{Path, PathBuf};

    use super::{
        WalkLimits, WalkStop, allowed_link_targets, first_outside_link, outside_link_refusal,
        walk_for_outside_link,
    };

    #[test]
    fn a_link_out_of_the_tree_is_found_and_one_inside_is_not() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let deep = root.path().join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.txt"), "x").unwrap();
        // Inside: allowed, and not entered.
        std::os::unix::fs::symlink(root.path().join("a"), deep.join("up")).unwrap();
        assert_eq!(first_outside_link(root.path(), &[]).unwrap(), None);
        // Outside, two levels down.
        let link = deep.join("out");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let (found, target) = first_outside_link(root.path(), &[]).unwrap().unwrap();
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
            first_outside_link(root.path(), &[]).unwrap(),
            Some((dangling, None))
        );
    }

    /// A worktree holding the dependency link kGraf made into the checkout:
    /// refused while the checkout's tree is not in the allowed set, allowed
    /// once it is. This is the grant a worktree gets when it comes back on
    /// another slot user, where the link is already there.
    #[test]
    fn the_dependency_link_kgraf_made_is_allowed_only_as_a_granted_target() {
        let tree = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let installed = checkout.path().join("node_modules");
        std::fs::create_dir_all(installed.join("pkg")).unwrap();
        let link = tree.path().join("node_modules");
        std::os::unix::fs::symlink(&installed, &link).unwrap();
        let found = first_outside_link(tree.path(), &[]).unwrap();
        assert_eq!(found.as_ref().map(|(l, _)| l.clone()), Some(link));
        let granted = vec![kgraf_core::plain_path::canonicalize(&installed).unwrap()];
        assert_eq!(first_outside_link(tree.path(), &granted).unwrap(), None);
    }

    /// A workspace monorepo: `node_modules/@scope/pkg` is a link to a
    /// package of the checkout, outside `node_modules`. A read grant on the
    /// checkout's own tree allows it, because the whole checkout is in the
    /// set for that grant.
    #[test]
    fn a_workspace_link_inside_the_checkouts_own_tree_is_allowed() {
        let checkout = tempfile::tempdir().unwrap();
        let checkout_root = kgraf_core::plain_path::canonicalize(checkout.path()).unwrap();
        let installed = checkout_root.join("node_modules");
        std::fs::create_dir_all(installed.join("@scope")).unwrap();
        std::fs::create_dir_all(checkout_root.join("packages").join("pkg")).unwrap();
        std::os::unix::fs::symlink(
            Path::new("..").join("..").join("packages").join("pkg"),
            installed.join("@scope").join("pkg"),
        )
        .unwrap();
        // Without the checkout in the set the walk refuses the grant.
        assert!(first_outside_link(&installed, &[]).unwrap().is_some());
        let trees = vec![installed.clone()];
        let allowed =
            allowed_link_targets(&installed, &trees, &trees, Some(checkout_root.as_path()));
        assert_eq!(first_outside_link(&installed, &allowed).unwrap(), None);
    }

    /// The checkout is in the set only for a grant on one of its own
    /// dependency trees, never for a grant on a worktree: a command may
    /// write in its worktree and could plant a link to the checkout root.
    #[test]
    fn the_checkout_is_allowed_only_for_a_grant_on_its_own_dependency_tree() {
        let tree = PathBuf::from("/w/tree");
        let checkout = PathBuf::from("/c");
        let trees = vec![checkout.join("node_modules")];
        let granted = vec![tree.clone(), trees[0].clone()];
        let for_tree = allowed_link_targets(&tree, &granted, &trees, Some(&checkout));
        assert!(!for_tree.contains(&checkout), "{for_tree:?}");
        assert!(for_tree.contains(&trees[0]), "{for_tree:?}");
        let for_installed = allowed_link_targets(&trees[0], &granted, &trees, Some(&checkout));
        assert!(for_installed.contains(&checkout), "{for_installed:?}");
        let deeper = trees[0].join("pkg");
        let for_deeper = allowed_link_targets(&deeper, &granted, &trees, Some(&checkout));
        assert!(for_deeper.contains(&checkout), "{for_deeper:?}");
    }

    /// A folder that cannot be read fails the walk by name, not as a
    /// failure of the root: one unreadable subfolder was reported as the
    /// root before, which named nothing the owner could act on.
    #[test]
    fn an_unreadable_folder_is_named_not_the_root() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let closed = root.path().join("a").join("closed");
        std::fs::create_dir_all(&closed).unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let stop = first_outside_link(root.path(), &[]).unwrap_err();
        // Restored before any assertion, so a failure still cleans up.
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        match &stop {
            WalkStop::Read { path, .. } => assert_eq!(path, &closed),
            other => panic!("{other:?}"),
        }
        let text = stop.to_string();
        assert!(text.contains("closed"), "{text}");
    }

    /// The walk has an entry bound and a time bound, and reaching either
    /// refuses the grant instead of running on. Without a bound a link to
    /// an unreachable network path holds the walk until the operating
    /// system gives up.
    #[test]
    fn the_walk_stops_at_its_entry_bound_and_at_its_time_bound() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..4 {
            std::fs::write(root.path().join(format!("f{n}")), "x").unwrap();
        }
        let tight = WalkLimits {
            entries: 2,
            bound: std::time::Duration::from_secs(60),
        };
        let stop = walk_for_outside_link(root.path(), &[], &tight).unwrap_err();
        match &stop {
            WalkStop::TooLarge { entries } => assert_eq!(*entries, 2),
            other => panic!("{other:?}"),
        }
        assert!(stop.to_string().contains("more than 2 entries"), "{stop}");
        let no_time = WalkLimits {
            entries: usize::MAX,
            bound: std::time::Duration::ZERO,
        };
        let stop = walk_for_outside_link(root.path(), &[], &no_time).unwrap_err();
        assert!(matches!(stop, WalkStop::TimedOut { .. }), "{stop:?}");
        assert!(stop.to_string().contains("past its bound"), "{stop}");
        // The default limits pass the same tree.
        assert_eq!(first_outside_link(root.path(), &[]).unwrap(), None);
    }

    /// A read root the walk refuses is reported as not granted, with the
    /// reason and the folder: the command runs, so a link of the owner's own
    /// in the toolchain does not stop every command.
    #[test]
    fn a_refused_read_root_is_a_note_that_names_the_folder_and_the_reason() {
        let note = super::refused_read_note(
            Path::new("/u/.rustup/toolchains"),
            "it holds the link x to y, which this command is not granted",
        );
        assert!(note.contains("/u/.rustup/toolchains"), "{note}");
        assert!(note.contains("is not granted"), "{note}");
        assert!(note.contains("a read there fails"), "{note}");
    }

    /// The refusal never tells the owner to remove a link kGraf itself made.
    #[test]
    fn the_refusal_does_not_ask_for_the_link_to_be_removed() {
        let text = outside_link_refusal(
            Path::new("/w/tree"),
            Path::new("/w/tree/node_modules"),
            Some(Path::new("/c/node_modules")),
        );
        assert!(!text.to_lowercase().contains("remove the link"), "{text}");
        assert!(text.contains("/c/node_modules"), "{text}");
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
pub(crate) use imp::{AttributeList, InheritWindow, live_jobs_holding, revoke};
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
        GetNamedSecurityInfoW, GetSecurityInfo, REVOKE_ACCESS, SE_FILE_OBJECT, SE_WINDOW_OBJECT,
        SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AccessCheck, AddAce, AddMandatoryAce,
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
        BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
        FILE_ATTRIBUTE_DIRECTORY, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_CHAR,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, GetFileInformationByHandle,
        GetFileType, GetVolumeInformationW, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
        PIPE_ACCESS_OUTBOUND, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
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
        JOB_OBJECT_UILIMIT_ALL, MAXIMUM_ALLOWED, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID,
        SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
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
    use crate::sandbox_user::acl;
    use crate::sandbox_user::acl::{Locations, closed_folder_of};
    use crate::sandbox_user::jobserver::JOBSERVER_VAR;
    use crate::sandbox_user::runner::Request;
    use crate::sandbox_user::slots::{AclLock, COMMAND_WAIT, Lease, Pool};
    use crate::sandbox_user::store::{credentials_path, slots_dir};
    use crate::sandbox_user::windows::jobserver::{self, Jobserver};
    use crate::sandbox_user::windows::state::{self, LoadedUser, Users};
    use crate::sandbox_user::windows::{accounts, launch, secure};
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

    /// kGraf's own user, read once: the owner of the worktrees whose access
    /// lists it writes. Named in a tree grant, because the `OWNER RIGHTS`
    /// entry there replaces the implicit right of an owner to rewrite a list
    /// ([`acl::tree_grant`]).
    fn owner_sid() -> Result<&'static Sid, SandboxError> {
        static SID: std::sync::OnceLock<Result<Sid, String>> = std::sync::OnceLock::new();
        SID.get_or_init(|| {
            crate::sandbox_user::windows::accounts::current_user_sid()
                .and_then(|text| Sid::from_string(&text))
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|why| SandboxError::PlatformSetup {
            step: "read this process's own user SID",
            source: io::Error::other(why.clone()),
        })
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

    /// The folder the access-list lock file lives in: this owner's slot
    /// folder, derived from the tag and the configuration folder rather than
    /// from the stored passwords, so it is the same file before and after
    /// `kgraf sandbox setup` has run, and the same for the token sandbox,
    /// whose grants are written on the same folders.
    fn acl_lock_dir() -> Result<&'static Path, SandboxError> {
        static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let tag = std::env::var(state::TAG_ENV).ok().filter(|t| !t.is_empty());
            credentials_path(tag.as_deref()).as_deref().map(slots_dir)
        })
        .as_deref()
        .ok_or_else(|| SandboxError::PlatformSetup {
            step: "find the folder of the sandbox access-list lock",
            source: io::Error::other(
                "no kGraf configuration folder was found, so the lock that keeps two \
                 processes from writing one folder's access list at the same time has \
                 nowhere to live",
            ),
        })
    }

    /// Hold the one lock around a read-modify-write of an access list
    /// ([`crate::sandbox_user::slots::lock_acls`]): read the list, merge the
    /// entries, write it back. Every function below that writes a list takes
    /// it, and re-reads the list under it, so no writer merges its entries
    /// into a list another writer has since replaced.
    fn lock_acls() -> Result<AclLock, SandboxError> {
        let dir = acl_lock_dir()?;
        crate::sandbox_user::slots::lock_acls(dir).map_err(|e| SandboxError::PlatformSetup {
            step: "take the sandbox access-list lock",
            source: io::Error::other(e.to_string()),
        })
    }

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

    /// A file object's mandatory label (its system list, read with
    /// `LABEL_SECURITY_INFORMATION`, which needs only `READ_CONTROL`, unlike
    /// the audit entries of the same list). The struct is [`Dacl`]: a label
    /// list holds entries of the same shape.
    fn read_label(path: &Path) -> Result<Dacl, SandboxError> {
        let name = wide(path.as_os_str());
        let mut acl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; outputs are written on success and
        // the descriptor is freed by `Local`.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
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

    /// A handle on `path` for a write of its own access list that propagates
    /// nothing to what already exists below it, and how that write is made
    /// through it ([`super::ListWrite`]).
    ///
    /// **First, `MAXIMUM_ALLOWED`.** Microsoft's `SetSecurityInfo` page
    /// states the rule this rests on: "If the supplied handle was opened with
    /// an ACCESS_MASK value of MAXIMUM_ALLOWED, then the SetSecurityInfo
    /// function will not propagate ACEs to children." (Windows API, aclapi.h,
    /// SetSecurityInfo, Warning; read 2026-10-08 and again 2026-10-09).
    /// Upstream openai/codex `codex-rs/windows-sandbox-rs` opens its handles
    /// the same way for the same reason. kGraf then walks the tree itself
    /// ([`propagate`]), because the kernel's own walk follows what it finds:
    /// a junction it may enter, and a hard link whose file is also named
    /// outside the tree (decision 0083).
    ///
    /// **Then, on a sharing violation, a handle no other open refuses.**
    /// `MAXIMUM_ALLOWED` on an object kGraf controls is given every right,
    /// `DELETE` and the data rights among them, and those take part in
    /// sharing checks. A program that holds a folder or file of the worktree
    /// open without `FILE_SHARE_DELETE` therefore makes this open fail with
    /// `ERROR_SHARING_VIOLATION` (os error 32), and a grant, or a revoke
    /// during `kgraf sandbox remove` or a slot move, failed with it. Windows
    /// and its services hold `C:\` and `C:\Windows\Tasks` that way (the
    /// setup's shared folders met it there); a process's current folder
    /// alone does not (measured on Windows Server 2025). The
    /// object is then opened again with
    /// `READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES` and full sharing
    /// ([`secure::open_for_lone_write`]), and its list is written with
    /// `SetKernelObjectSecurity`, read back and rolled back on a difference
    /// ([`secure::write_dacl_alone`]), because `SetSecurityInfo` through a
    /// handle that is not `MAXIMUM_ALLOWED` would propagate. The common case
    /// keeps the first open, whose behavior Windows runs have measured; the
    /// second is taken only where the first is refused
    /// ([`super::lone_write_after`]).
    ///
    /// `FILE_FLAG_BACKUP_SEMANTICS` is what opens a folder at all, and
    /// `FILE_FLAG_OPEN_REPARSE_POINT` opens the name itself, so a junction
    /// is never followed; both opens use both. Neither protection flag
    /// (`PROTECTED_DACL_SECURITY_INFORMATION`,
    /// `UNPROTECTED_DACL_SECURITY_INFORMATION`) is passed to the write, and
    /// the lone write carries the object's own protected and auto-inherited
    /// bits, so the object keeps the inheritance protection Windows set on
    /// it.
    ///
    /// The share mode lets other programs keep the file open while kGraf
    /// writes its list.
    fn open_for_security(path: &Path) -> Result<ListHandle, SandboxError> {
        let code = match open_maximum_allowed(path) {
            Ok(handle) => {
                return Ok(ListHandle {
                    handle,
                    write: super::ListWrite::SetSecurityInfo,
                });
            }
            Err(code) => code,
        };
        if !super::lone_write_after(code) {
            return Err(acl_error(path, code));
        }
        let handle = secure::open_for_lone_write(path).map_err(|source| SandboxError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(ListHandle {
            handle,
            write: super::ListWrite::KernelObjectAlone,
        })
    }

    /// `path` opened with `MAXIMUM_ALLOWED`, full sharing and no link
    /// followed, or the Win32 error of the open.
    fn open_maximum_allowed(path: &Path) -> Result<OwnedHandle, u32> {
        let name = wide(path.as_os_str());
        // SAFETY: `name` is NUL-terminated; the call returns a handle or an
        // invalid-handle value.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                MAXIMUM_ALLOWED,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            // SAFETY: plain call, made right after the failing one.
            return Err(unsafe { GetLastError() });
        }
        // SAFETY: a valid handle this process now owns alone.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// A handle for a write of one object's access list, and how the write
    /// through it stays on that object ([`open_for_security`]).
    struct ListHandle {
        handle: OwnedHandle,
        write: super::ListWrite,
    }

    /// `dwFileAttributes` and `nNumberOfLinks` of whatever `handle` reached,
    /// in one call on the open [`propagate`] already makes. The kernel's own
    /// propagation opens each child too, so the pass replaces that cost
    /// instead of adding one.
    fn handle_info(handle: &OwnedHandle, path: &Path) -> Result<(u32, u32), SandboxError> {
        // SAFETY: a zeroed POD struct the call fills.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid handle and a buffer of the right size.
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
            return Err(SandboxError::Io {
                path: path.to_path_buf(),
                source: io::Error::last_os_error(),
            });
        }
        Ok((info.dwFileAttributes, info.nNumberOfLinks))
    }

    /// The DACL of whatever `handle` reached. The handle was opened without
    /// following a reparse point, so this is the list of the name itself.
    fn read_dacl_handle(handle: &OwnedHandle, path: &Path) -> Result<Dacl, SandboxError> {
        let mut acl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: a valid handle; outputs are written on success and the
        // descriptor is freed by `Local`.
        let code = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
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

    /// The mandatory label of whatever `handle` reached ([`read_label`] by
    /// handle).
    fn read_label_handle(handle: &OwnedHandle, path: &Path) -> Result<Dacl, SandboxError> {
        let mut acl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: a valid handle; outputs are written on success and the
        // descriptor is freed by `Local`.
        let code = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
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

    /// Merge `entries` into `dacl` and write the result as the list of
    /// whatever `handle` reached, with no propagation to existing children
    /// ([`open_for_security`]).
    ///
    /// `dacl` must have been read under the access-list lock ([`lock_acls`]),
    /// which every caller holds: the merge keeps whatever the list held when
    /// it was read, so a list read before another writer's write would put
    /// that writer's entries back as they were.
    fn apply_handle(
        handle: &ListHandle,
        path: &Path,
        dacl: &Dacl,
        entries: &[EXPLICIT_ACCESS_W],
    ) -> Result<(), SandboxError> {
        apply_handle_keeping(handle, path, dacl, entries, &|_| true)
    }

    /// [`apply_handle`], keeping of the merged list only the entries `keep`
    /// accepts (a removal's lone write on a child, [`remove_child_entries`]).
    fn apply_handle_keeping(
        handle: &ListHandle,
        path: &Path,
        dacl: &Dacl,
        entries: &[EXPLICIT_ACCESS_W],
        keep: &dyn Fn(*mut c_void) -> bool,
    ) -> Result<(), SandboxError> {
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
        let kept = kept_entries(new_acl, keep)?;
        let acl: *const ACL = match &kept {
            Some(buf) => buf.as_ptr().cast(),
            None => new_acl,
        };
        match handle.write {
            super::ListWrite::SetSecurityInfo => {
                // SAFETY: a valid handle and a valid ACL.
                let code = unsafe {
                    SetSecurityInfo(
                        handle.handle.as_raw_handle(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        acl,
                        ptr::null(),
                    )
                };
                if code != ERROR_SUCCESS {
                    return Err(acl_error(path, code));
                }
                Ok(())
            }
            super::ListWrite::KernelObjectAlone => {
                secure::write_dacl_alone(&handle.handle, acl, path)
            }
        }
    }

    /// A copy of `acl` holding only the entries `keep` accepts, in their
    /// order, or `None` when it accepts every one (or there is no list), so
    /// the common write uses the merged list as it is.
    fn kept_entries(
        acl: *mut ACL,
        keep: &dyn Fn(*mut c_void) -> bool,
    ) -> Result<Option<Vec<u32>>, SandboxError> {
        if acl.is_null() {
            return Ok(None);
        }
        // SAFETY: a valid list the caller owns.
        let (size, revision, count) = unsafe {
            (
                usize::from((*acl).AclSize),
                (*acl).AclRevision,
                (*acl).AceCount,
            )
        };
        let mut all = Vec::with_capacity(usize::from(count));
        for i in 0..u32::from(count) {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `i` is below the entry count of a valid list.
            if unsafe { GetAce(acl, i, &mut ace) } == 0 {
                return Err(last_error("read an access list entry"));
            }
            all.push(ace);
        }
        if all.iter().all(|ace| keep(*ace)) {
            return Ok(None);
        }
        // The copy is no larger than the list it is made from.
        let mut buf = vec![0u32; size.div_ceil(4)];
        let out: *mut ACL = buf.as_mut_ptr().cast();
        // SAFETY: `buf` holds `size` bytes, aligned for an ACL.
        if unsafe { InitializeAcl(out, size as u32, u32::from(revision)) } == 0 {
            return Err(last_error("build an access list"));
        }
        for ace in all.into_iter().filter(|ace| keep(*ace)) {
            // SAFETY: every entry starts with a header that gives its size.
            let len = u32::from(unsafe { (*(ace as *const ACE_HEADER)).AceSize });
            // SAFETY: appends one valid entry of `len` bytes to a list with
            // room for it, at the end, so the order is kept.
            if unsafe { AddAce(out, u32::from(revision), u32::MAX, ace, len) } == 0 {
                return Err(last_error("build an access list"));
            }
        }
        Ok(Some(buf))
    }

    /// Merge `entries` into the DACL of `path` and write it back, without
    /// propagating to what already exists below it
    /// ([`open_for_security`]). A caller whose entries are inheritable
    /// propagates them itself ([`apply_inheritable`]); callers skip this
    /// altogether when the entries are already present.
    fn apply(path: &Path, dacl: &Dacl, entries: &[EXPLICIT_ACCESS_W]) -> Result<(), SandboxError> {
        let handle = open_for_security(path)?;
        apply_handle(&handle, path, dacl, entries)
    }

    /// What one existing child of a tree gets: a folder the same
    /// inheritable pair the root carries, a file the effective entry.
    const CHILD_INHERIT: u32 = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;

    /// Write one entry on an existing child of a tree kGraf just granted or
    /// denied: `mask` with `mode` ([`GRANT_ACCESS`], [`DENY_ACCESS`]) for
    /// `sid`, inheritable on a folder so that everything made in it later
    /// gets it from Windows, effective only on a file.
    ///
    /// **This entry is explicit, not an inherited copy.** Windows would mark
    /// a propagated entry `INHERITED_ACE` and derive its mask and flags from
    /// the parent's entry; kGraf writes one plain entry instead, which is at
    /// least as wide as anything that derivation gives. That is the owner's
    /// choice (2026-10-08): excessive permission inside the tree over a
    /// failed run. It grants nothing beyond the tree, because the whole tree
    /// is granted to this user already, and the two rules that hold the tree
    /// boundary are elsewhere ([`super::child_action`]): a reparse point is
    /// not entered and not written, and a file with more than one hard link
    /// is never written.
    ///
    /// Two consequences to know. `icacls` shows these entries as explicit on
    /// every folder and file of the tree, not as inherited copies, so its
    /// output differs from the restricted-token backend's. And a later write
    /// of the parent's list does not remove them, so the removal pass takes
    /// them off itself ([`revoke`]).
    ///
    /// Already-present entries are not written again, so a second pass over
    /// the same tree costs one list read per child.
    fn write_child_entry(
        handle: &ListHandle,
        path: &Path,
        is_dir: bool,
        sid: &Sid,
        mask: u32,
        mode: i32,
        ace_type: u32,
    ) -> Result<(), SandboxError> {
        let flags = if is_dir {
            CHILD_INHERIT
        } else {
            NO_INHERITANCE
        };
        write_child_entries(
            handle,
            path,
            &[explicit(sid, mask, mode, flags)],
            &|d: &Dacl| has_ace(d, ace_type, sid, mask, flags),
        )
    }

    /// Write `entries` on an existing child of a tree, as
    /// [`write_child_entry`] writes one: nothing is written while `done`
    /// reports the list already holds them.
    ///
    /// A grant writes several entries on one child, not one: the set is the
    /// one the root carries, mapped onto this child
    /// ([`acl::child_grant`]), so the `OWNER RIGHTS` restriction and kGraf's
    /// compensating entry reach a file that existed before the grant. The
    /// kernel used to carry them there and no longer does
    /// ([`open_for_security`]).
    ///
    /// The access-list lock is held for this one read-modify-write, as it is
    /// for every other ([`lock_acls`]), and not across the pass: a pass over
    /// a large tree lasts as long as the walk, and the hazard the lock exists
    /// for is one list, read and written by two writers. Taking it again
    /// inside a pass that already holds it costs a counter, because it is
    /// reentrant within a thread.
    fn write_child_entries(
        handle: &ListHandle,
        path: &Path,
        entries: &[EXPLICIT_ACCESS_W],
        done: &dyn Fn(&Dacl) -> bool,
    ) -> Result<(), SandboxError> {
        let _acl = lock_acls()?;
        let dacl = read_dacl_handle(&handle.handle, path)?;
        if done(&dacl) {
            return Ok(());
        }
        apply_handle(handle, path, &dacl, entries)
    }

    /// Take every entry for `sids` off an existing child of a tree whose
    /// grant is being removed ([`revoke`]). A removal only narrows access,
    /// so it needs no entry of its own and cannot carry anything out of the
    /// tree.
    ///
    /// A child is written only when [`super::removal_writes`] says so: a
    /// removal walks the whole tree, and a worktree with a built `target`
    /// folder holds hundreds of thousands of files whose lists have nothing
    /// to do with this user. Through a lone write (a child another handle
    /// holds without delete sharing) the inherited copies of the removed entries go
    /// too ([`super::removal_keeps`]), because that write, unlike
    /// `SetSecurityInfo`, takes nothing in from the parent.
    fn remove_child_entries(
        handle: &ListHandle,
        path: &Path,
        sids: &[Sid],
    ) -> Result<(), SandboxError> {
        let dacl = read_dacl_handle(&handle.handle, path)?;
        let facts: Vec<super::EntryFacts> = dacl_entries(dacl.acl)
            .into_iter()
            .map(|ace| entry_facts(ace, sids))
            .collect();
        if !super::removal_writes(handle.write, &facts) {
            return Ok(());
        }
        let entries: Vec<EXPLICIT_ACCESS_W> = sids
            .iter()
            .map(|sid| explicit(sid, 0, REVOKE_ACCESS, NO_INHERITANCE))
            .collect();
        apply_handle_keeping(handle, path, &dacl, &entries, &|ace| {
            super::removal_keeps(handle.write, entry_facts(ace, sids))
        })
    }

    /// The entries of `acl`, in list order; none for a missing list.
    fn dacl_entries(acl: *mut ACL) -> Vec<*mut c_void> {
        if acl.is_null() {
            return Vec::new();
        }
        // SAFETY: `acl` points into a descriptor the caller holds.
        let count = unsafe { (*acl).AceCount };
        (0..u32::from(count))
            .filter_map(|i| {
                let mut ace: *mut c_void = ptr::null_mut();
                // SAFETY: `i` is below the entry count of a valid list.
                (unsafe { GetAce(acl, i, &mut ace) } != 0).then_some(ace)
            })
            .collect()
    }

    /// One entry as a removal judges it ([`super::EntryFacts`]). Only allow
    /// and deny entries are read for a SID: they share the layout read here,
    /// and a file system list holds no other kind kGraf writes.
    fn entry_facts(ace: *mut c_void, sids: &[Sid]) -> super::EntryFacts {
        const INHERITED_ACE: u32 = 0x10;
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        let kind = u32::from(header.AceType);
        let plain = kind == ACCESS_ALLOWED_ACE_TYPE || kind == ACCESS_DENIED_ACE_TYPE;
        let names_removed = plain && {
            // SAFETY: allow and deny entries share this layout.
            let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
            // SAFETY: both are valid SIDs.
            sids.iter()
                .any(|sid| unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0)
        };
        super::EntryFacts {
            allow: kind == ACCESS_ALLOWED_ACE_TYPE,
            inherited: u32::from(header.AceFlags) & INHERITED_ACE != 0,
            names_removed,
        }
    }

    /// kGraf's own propagation pass over the existing children of `root`:
    /// one open per child, the decision of [`super::child_action`] on what
    /// that open reports, and `write` on the children it may write.
    /// `denied` names folders no entry of this pass may land in (the
    /// top-level `.git`, the policy's hidden folders), which are not entered
    /// either.
    ///
    /// The pass is made instead of the kernel's, which is suppressed
    /// ([`open_for_security`]), because the kernel follows what it finds:
    /// it may enter a junction, and it writes the one security descriptor a
    /// hard-linked file shares with every other name of it, which can lie
    /// outside the tree (decision 0083).
    ///
    /// A child another process holds open without delete sharing (a current
    /// folder, a file open in a build) is written through a second handle
    /// that no open refuses ([`open_for_security`]). A child it still cannot
    /// open or write is recorded and the pass goes on, as Windows' own
    /// propagation does with a child whose share access is not available
    /// (`super::Skipped::blocked`); the note reaches the command's output, so
    /// a file that keeps its old list is never silent. A folder that cannot
    /// be listed is recorded the same way and not entered.
    ///
    /// No time or entry bound: the walk in front of the write is bounded and
    /// refuses a tree larger than that bound, and this pass must finish once
    /// the folder's own list is written. It enters no reparse point, so no
    /// unreachable network path can hold it.
    ///
    /// The walk itself is [`super::propagation_plan`], which runs on any
    /// host; this adds the three Windows operations: list a folder, open one
    /// child and read its attributes and link count in that same open, and
    /// write the entry through that handle.
    fn propagate(
        root: &Path,
        denied: &[PathBuf],
        write: &dyn Fn(&ListHandle, &Path, bool) -> Result<(), SandboxError>,
    ) -> Result<super::Skipped, SandboxError> {
        let mut list = |dir: &Path| -> Result<Vec<PathBuf>, String> {
            let mut children = Vec::new();
            for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
                children.push(entry.map_err(|e| e.to_string())?.path());
            }
            Ok(children)
        };
        // Each reason names the step that failed (the open, its reading, the
        // write), so a note that lists a child says which call refused it.
        let mut probe = |path: &Path| -> Result<super::ChildFacts<ListHandle>, String> {
            let handle = open_for_security(path).map_err(|e| format!("open: {e}"))?;
            let (attributes, links) =
                handle_info(&handle.handle, path).map_err(|e| format!("read: {e}"))?;
            Ok(super::ChildFacts {
                is_reparse: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                links,
                handle,
            })
        };
        let mut write_one = |handle: &ListHandle, path: &Path, is_dir: bool| {
            let how = match handle.write {
                super::ListWrite::SetSecurityInfo => "write (SetSecurityInfo)",
                super::ListWrite::KernelObjectAlone => "write (lone)",
            };
            write(handle, path, is_dir).map_err(|e| format!("{how}: {e}"))
        };
        super::propagation_plan(root, denied, &mut list, &mut probe, &mut write_one).map_err(
            |why| SandboxError::PlatformSetup {
                step: "write the sandbox user's entry on what a granted tree already holds",
                source: io::Error::other(format!("{} could not be listed: {why}", root.display())),
            },
        )
    }

    /// Why an inheritable entry must not be written on the tree `root`, or
    /// `None` when it may be: the walk found a link that leads neither
    /// inside `root` nor into one of `allowed`
    /// ([`super::first_outside_link`]), or it could not finish. The write
    /// propagates to what already exists below, and could follow the link.
    fn link_walk_refusal(root: &Path, allowed: &[PathBuf]) -> Option<String> {
        match super::first_outside_link(root, allowed) {
            Ok(None) => None,
            Ok(Some((link, target))) => {
                Some(super::outside_link_refusal(root, &link, target.as_deref()))
            }
            Err(e) => Some(format!(
                "{} could not be checked for links that lead out of it ({e}), so the sandbox \
                 user's entry was not written on it",
                root.display()
            )),
        }
    }

    /// Merge `entries`, which are inheritable, into the access list of the
    /// tree `dir`, then write the same access on each existing child
    /// ([`propagate`]), with the link walk before and after
    /// ([`super::guarded_inheritable_write`]). Every inheritable write on a
    /// folder goes through this: it reaches everything already below, and a
    /// command that may write in the folder can plant a junction between the
    /// walk and the pass.
    ///
    /// The folder's own entries stay inheritable, so Windows gives every
    /// child made later the right entry by itself; only the children that
    /// exist now are written here, and `write_child` writes the same entries
    /// on each ([`write_child_entries`]).
    ///
    /// The folder's own list is read here, under the access-list lock
    /// ([`lock_acls`]), and not by the caller: a list read before another
    /// process wrote its own entries would carry them away again on the write
    /// back. `done` reports a list that already holds the entries, which
    /// another process may have written while this one walked; the pass still
    /// runs, because a child can lack what the folder has.
    ///
    /// The lock is held for the folder's own read-modify-write and for each
    /// child's, never across the whole pass or across a walk: a pass over a
    /// large tree takes as long as the walk does, and the hazard the lock
    /// exists for is one list, read and written by two writers.
    fn apply_inheritable(
        dir: &Path,
        entries: &[EXPLICIT_ACCESS_W],
        done: &dyn Fn(&Dacl) -> bool,
        allowed: &[PathBuf],
        denied: &[PathBuf],
        write_child: &dyn Fn(&ListHandle, &Path, bool) -> Result<(), SandboxError>,
    ) -> Result<super::Skipped, super::GuardedWrite<SandboxError>> {
        super::guarded_inheritable_write(
            || link_walk_refusal(dir, allowed),
            || {
                {
                    let _acl = lock_acls()?;
                    let dacl = read_dacl(dir)?;
                    if !done(&dacl) {
                        apply(dir, &dacl, entries)?;
                    }
                }
                propagate(dir, denied, write_child)
            },
        )
    }

    /// The error a guarded inheritable write failed with.
    fn planted_or_refused(dir: &Path, e: super::GuardedWrite<SandboxError>) -> SandboxError {
        match e {
            super::GuardedWrite::Refused(why) => SandboxError::PlatformSetup {
                step: "check a tree for links before granting on it",
                source: io::Error::other(why),
            },
            super::GuardedWrite::Write(e) => e,
            super::GuardedWrite::PlantedAfterWrite(why) => SandboxError::PlatformSetup {
                step: "check a tree for links after granting on it",
                source: io::Error::other(format!(
                    "the entry was written on {} and the link below it was found only \
                     afterwards, so the entry may already have been carried onto the link's \
                     target: {why}",
                    dir.display()
                )),
            },
        }
    }

    /// Admit `sid` to write beneath `root`. The owner of an object may
    /// rewrite its DACL, so this needs no administrator rights for a
    /// user-owned worktree; with `confine_owners` that right comes from
    /// kGraf's own entry in the list instead, which the same write puts
    /// there. Verified by reading the DACL back (fail loud).
    /// The first grant for `sid` is refused while a link in the tree leads
    /// neither inside it nor into one of `allowed`
    /// ([`apply_inheritable`], [`super::allowed_link_targets`]). `denied`
    /// names the folders this grant must not reach (the top-level `.git`,
    /// the policy's hidden folders): the pass over the existing children does
    /// not enter them, so the deny entries written on them afterwards hold.
    ///
    /// The access-list lock is held from the read to the write
    /// ([`apply_inheritable`], [`lock_acls`]), so two processes that grant
    /// two users on one folder each keep the other's entry. The link walk
    /// runs before the lock, because it reads the whole tree, and the list is
    /// read again under the lock, immediately before the write.
    /// With `confine_owners`, the grant also carries the two owner entries
    /// of [`acl::tree_grant`], which keep a sandbox user that owns the files
    /// it created from giving itself rights a revoke took away. Only the
    /// sandbox-user backend passes it: under the token sandbox every command
    /// runs as kGraf's own user, so ownership of a file in the tree gives no
    /// other principal anything.
    ///
    /// **One entry set, at the root and on every existing child.** The set is
    /// derived once here from [`acl::tree_grant`], and the pass writes the
    /// same set, mapped onto one child by [`acl::child_grant`], on each child
    /// it may write. The kernel's own propagation, which used to carry the
    /// root's inheritable entries to the children that already existed, is
    /// suppressed ([`open_for_security`]), so a set written at the root alone
    /// would leave every file already in the worktree without the
    /// `OWNER RIGHTS` restriction and without kGraf's compensating entry:
    /// the sandbox user owns the files it creates, and an owner may rewrite
    /// the list of what it owns whatever that list says.
    fn grant_writable_root(
        root: &Path,
        sid: &Sid,
        allowed: &[PathBuf],
        denied: &[PathBuf],
        confine_owners: bool,
    ) -> Result<super::Skipped, SandboxError> {
        let aces = if confine_owners {
            acl::tree_grant(ROOT_MASK, TREE_MASK)
        } else {
            vec![
                acl::TreeAce {
                    who: acl::Who::SandboxUser,
                    mask: ROOT_MASK,
                    scope: acl::Scope::Folder,
                },
                acl::TreeAce {
                    who: acl::Who::SandboxUser,
                    mask: TREE_MASK,
                    scope: acl::Scope::Below,
                },
            ]
        };
        // The trustees a tree grant can name, owned here, so every entry
        // built below may point at one of them for as long as it lives.
        let owner_rights = Sid::from_string(acl::OWNER_RIGHTS_SID)?;
        let me = if aces.iter().any(|a| a.who == acl::Who::Owner) {
            Some(owner_sid()?)
        } else {
            None
        };
        let sid_of = |who: acl::Who| -> &Sid {
            match who {
                acl::Who::SandboxUser => sid,
                acl::Who::OwnerRights => &owner_rights,
                // Present whenever an entry names it, by the line above.
                acl::Who::Owner => me.expect("kGraf's own SID"),
            }
        };
        let flags = |scope: acl::Scope| match scope {
            acl::Scope::Folder => NO_INHERITANCE,
            acl::Scope::Below => TREE_INHERIT,
            acl::Scope::HereAndBelow => CHILD_INHERIT,
        };
        let entries_of = |aces: &[acl::TreeAce]| -> Vec<EXPLICIT_ACCESS_W> {
            aces.iter()
                .map(|ace| explicit(sid_of(ace.who), ace.mask, GRANT_ACCESS, flags(ace.scope)))
                .collect()
        };
        let holds = |d: &Dacl, aces: &[acl::TreeAce]| {
            aces.iter().all(|ace| {
                has_ace(
                    d,
                    ACCESS_ALLOWED_ACE_TYPE,
                    sid_of(ace.who),
                    ace.mask,
                    flags(ace.scope),
                )
            })
        };
        let present = |d: &Dacl| holds(d, &aces);
        if present(&read_dacl(root)?) {
            return Ok(super::Skipped::default());
        }
        let entries = entries_of(&aces);
        // The walk, then the root's list read and written under the
        // access-list lock, then the pass over the children, each child's
        // list read and written under that lock too, then the walk again.
        let skipped = apply_inheritable(
            root,
            &entries,
            &present,
            allowed,
            denied,
            &|handle, path, is_dir| {
                let child = acl::child_grant(&aces, is_dir);
                write_child_entries(handle, path, &entries_of(&child), &|d: &Dacl| {
                    holds(d, &child)
                })
            },
        )
        .map_err(|e| planted_or_refused(root, e))?;
        if !present(&read_dacl(root)?) {
            return Err(SandboxError::PlatformSetup {
                step: "verify writable-root ACEs",
                source: io::Error::other(format!(
                    "the allow ACEs for {} are missing after SetSecurityInfo",
                    root.display()
                )),
            });
        }
        Ok(skipped)
    }

    // ---- ACLs for the sandbox user (decision 0097) ---------------------------

    /// Read and run: what the sandbox user gets on the folders it reads.
    const READ_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    /// What a hidden folder denies the sandbox user: reads and writes. Deny
    /// on read takes effect here, unlike under the write-restricted token,
    /// whose restricting SIDs are consulted for writes only.
    const DENY_ALL_MASK: u32 = READ_MASK | DENY_WRITE_MASK;

    /// What one read grant did.
    enum ReadGrant {
        /// The entry was written now, with what the pass over the folder's
        /// existing children left alone.
        Written(super::Skipped),
        /// The entry was already there.
        Present,
        /// The link walk refused the folder, and why: nothing was written.
        Refused(String),
    }

    /// Admit `sid` to read and run `path` and, for a folder, everything
    /// below it. Written only while absent, as [`grant_writable_root`] is.
    /// A reparse point is refused first, so the entry never lands on a
    /// link's target (decision 0083). For a folder, a link beneath it that
    /// leads neither inside it nor into one of `allowed` refuses the folder
    /// ([`apply_inheritable`]).
    ///
    /// A refusal is `Refused`, not an error: a read root that is not granted
    /// takes nothing away from the sandbox, and the caller reports it with
    /// the command's output. The owner's toolchain reads are the case that
    /// matters: a toolchain linked into the rustup home
    /// (`rustup toolchain link`) or a link in a cargo registry crate is the
    /// owner's own, and failing the command would stop every command on the
    /// machine. An entry that was written and may have been carried through
    /// a link planted during the write stays an error.
    fn grant_read(path: &Path, sid: &Sid, allowed: &[PathBuf]) -> Result<ReadGrant, SandboxError> {
        refuse_reparse(path)?;
        let flags = if path.is_dir() {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            NO_INHERITANCE
        };
        let present = |d: &Dacl| has_ace(d, ACCESS_ALLOWED_ACE_TYPE, sid, READ_MASK, flags);
        if present(&read_dacl(path)?) {
            return Ok(ReadGrant::Present);
        }
        let entries = [explicit(sid, READ_MASK, GRANT_ACCESS, flags)];
        let mut skipped = super::Skipped::default();
        if path.is_dir() {
            // As in `grant_writable_root`: the walk runs outside the
            // access-list lock, each list is read and written under it.
            // Without the lock, four slot users granted on one toolchain
            // folder at the same time would each write a list holding its own
            // entry alone, and the last writer would take the other three
            // grants away.
            //
            // Nothing is denied inside a read root: the folders kGraf denies
            // the sandbox user lie inside its writable roots.
            match apply_inheritable(
                path,
                &entries,
                &present,
                allowed,
                &[],
                &|handle, child, is_dir| {
                    write_child_entry(
                        handle,
                        child,
                        is_dir,
                        sid,
                        READ_MASK,
                        GRANT_ACCESS,
                        ACCESS_ALLOWED_ACE_TYPE,
                    )
                },
            ) {
                Ok(pass) => skipped = pass,
                Err(super::GuardedWrite::Refused(why)) => return Ok(ReadGrant::Refused(why)),
                Err(e) => return Err(planted_or_refused(path, e)),
            }
        } else {
            // A file's entry is not inheritable, so nothing propagates and
            // no tree is walked. The lock is the same one all the same: two
            // processes can read and write one file's list at once.
            let _acl = lock_acls()?;
            let dacl = read_dacl(path)?;
            if present(&dacl) {
                return Ok(ReadGrant::Present);
            }
            apply(path, &dacl, &entries)?;
        }
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
                    "the read entry for {} is missing after SetSecurityInfo",
                    path.display()
                )),
            });
        }
        Ok(ReadGrant::Written(skipped))
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
            let err = grant_writable_root(root.path(), &sid, &[], &[], false).unwrap_err();
            assert!(err.to_string().contains("is not granted"), "{err}");
            assert!(!mentions(root.path(), &sid), "nothing written on the root");
            assert!(!mentions(outside.path(), &sid));
            assert!(!mentions(&outside.path().join("owner.txt"), &sid));
            // A read root is refused with its reason, not as an error: it is
            // not granted, and the command says so.
            match grant_read(root.path(), &sid, &[]).unwrap() {
                ReadGrant::Refused(why) => assert!(why.contains("is not granted"), "{why}"),
                _ => panic!("the read grant was not refused"),
            }
            assert!(!mentions(outside.path(), &sid));
        }

        #[test]
        fn a_junction_that_stays_in_the_tree_does_not_stop_the_grant() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("store")).unwrap();
            junction(&root.path().join("jn"), &root.path().join("store"));
            grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            assert!(mentions(root.path(), &sid));
        }

        /// Every other inheritable write on a tree is guarded as a grant is:
        /// a deny entry and the Low label carried through a junction would
        /// write the owner's own files outside the tree.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn a_deny_and_the_low_label_on_a_tree_with_a_junction_out_of_it_are_refused() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("owner.txt"), "x").unwrap();
            junction(&root.path().join("jn"), outside.path());
            for err in [
                deny_writes(root.path(), &sid, &[]).unwrap_err(),
                deny_all(root.path(), &sid, &[]).unwrap_err(),
                label_root_low(root.path(), &[]).unwrap_err(),
            ] {
                assert!(err.to_string().contains("is not granted"), "{err}");
            }
            assert!(!mentions(outside.path(), &sid));
        }

        /// The Low label is written only while it is absent, so a command
        /// does not rewrite the whole tree's labels, and the walk that
        /// guards the write runs only then.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn the_low_label_is_written_once_and_then_only_read() {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            label_root_low(root.path(), &[]).unwrap();
            junction(&root.path().join("jn"), outside.path());
            // Present now, so no write and no walk: the junction does not
            // refuse it.
            label_root_low(root.path(), &[]).unwrap();
        }

        /// A junction to a folder the same grant admits the same user to
        /// (a dependency tree kGraf linked into the worktree) does not stop
        /// the grant, so a worktree that comes back on another slot user
        /// still runs.
        #[test]
        fn a_junction_to_an_allowed_target_does_not_stop_the_grant() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let checkout = tempfile::tempdir().unwrap();
            let installed = checkout.path().join("node_modules");
            std::fs::create_dir_all(&installed).unwrap();
            junction(&root.path().join("node_modules"), &installed);
            let allowed = vec![kgraf_core::plain_path::canonicalize(&installed).unwrap()];
            grant_writable_root(root.path(), &sid, &allowed, &[], false).unwrap();
            assert!(mentions(root.path(), &sid));
        }

        /// Every entry of `path`'s list that names `sid`, in list order, as
        /// `(ace_type, inheritance flags)`.
        fn entries_for(path: &Path, sid: &Sid) -> Vec<(u32, u32)> {
            let dacl = read_dacl(path).unwrap();
            if dacl.acl.is_null() {
                return Vec::new();
            }
            // SAFETY: a valid list `dacl` owns.
            let count = unsafe { (*dacl.acl).AceCount };
            (0..u32::from(count))
                .filter_map(|i| {
                    let mut ace: *mut c_void = ptr::null_mut();
                    // SAFETY: `i` is below the entry count.
                    if unsafe { GetAce(dacl.acl, i, &mut ace) } == 0 {
                        return None;
                    }
                    // SAFETY: every entry starts with an ACE_HEADER.
                    let header = unsafe { &*(ace as *const ACE_HEADER) };
                    // SAFETY: allow and deny entries share this layout.
                    let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
                    let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
                    // SAFETY: both are valid SIDs.
                    if unsafe { EqualSid(ace_sid, sid.as_psid()) } == 0 {
                        return None;
                    }
                    Some((u32::from(header.AceType), u32::from(header.AceFlags)))
                })
                .collect()
        }

        /// The grant pass writes a plain allow entry on every existing
        /// child, so a deny written afterwards on a hidden folder has to
        /// stand before it in the child's list, or the allow would win.
        /// `SetEntriesInAclW` is what keeps that order.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn a_deny_on_a_child_stands_before_the_grant_pass_entry() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let hidden = root.path().join("secrets");
            std::fs::create_dir_all(&hidden).unwrap();
            let inside = hidden.join("key.txt");
            std::fs::write(&inside, "x").unwrap();
            // The grant reaches the hidden folder only when it is not named
            // as denied, which is the case `prepare` avoids; this test takes
            // that case on purpose, so the order is checked where both
            // entries meet.
            grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            // The file holds the pass's own entry and, once the pass has
            // written the folder above it, an entry inherited from there:
            // writing the file's list with `SetSecurityInfo` takes in the
            // parent's inheritable entries, as Windows documents for a list
            // that is not protected. Both allow; the pass's is the explicit
            // one, and there is exactly one of it.
            const INHERITED_ACE: u32 = 0x10;
            let granted = entries_for(&inside, &sid);
            assert_eq!(
                granted
                    .iter()
                    .filter(|(kind, flags)| *kind == ACCESS_ALLOWED_ACE_TYPE
                        && flags & INHERITED_ACE == 0)
                    .count(),
                1,
                "the grant pass entry: {granted:?}"
            );
            assert!(
                granted
                    .iter()
                    .all(|(kind, _)| *kind == ACCESS_ALLOWED_ACE_TYPE),
                "{granted:?}"
            );
            deny_all(&hidden, &sid, &[]).unwrap();
            let entries = entries_for(&inside, &sid);
            let deny = entries
                .iter()
                .position(|(kind, _)| *kind == ACCESS_DENIED_ACE_TYPE)
                .expect("a deny entry");
            // Every allow, the explicit one and any inherited one, stands
            // after the deny, or the first allow would decide the check.
            let allows: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, (kind, _))| *kind == ACCESS_ALLOWED_ACE_TYPE)
                .map(|(i, _)| i)
                .collect();
            assert!(!allows.is_empty(), "an allow entry: {entries:?}");
            assert!(
                allows.iter().all(|a| deny < *a),
                "the deny must come first: {entries:?}"
            );
        }

        /// The gap this pass closes: a file in the tree with a second hard
        /// link outside it shares one security descriptor with that name, so
        /// its list is never written. pnpm and uv hard-link package files
        /// from a shared store into a project.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn a_hard_linked_file_in_the_tree_never_gets_the_grant() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let store = tempfile::tempdir().unwrap();
            let stored = store.path().join("pkg.js");
            std::fs::write(&stored, "x").unwrap();
            let linked = root.path().join("node_modules_pkg.js");
            std::fs::create_dir_all(root.path().join("src")).unwrap();
            let plain = root.path().join("src").join("main.rs");
            std::fs::write(&plain, "fn main() {}").unwrap();
            std::fs::hard_link(&stored, &linked).unwrap();
            let skipped = grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            // The store file keeps its own list, under both of its names.
            assert!(
                !mentions(&stored, &sid),
                "the owner's store file was written"
            );
            assert!(!mentions(&linked, &sid));
            // A file with one link, and every folder, got the entry.
            assert!(mentions(&plain, &sid));
            assert!(mentions(&root.path().join("src"), &sid));
            assert_eq!(skipped.hard_links, 1);
            assert_eq!(skipped.first_hard_links, vec![linked]);
            let note = skipped
                .note(root.path(), "the write grant")
                .expect("a note names the file");
            assert!(note.contains("another hard link"), "{note}");
        }

        /// A child carries kGraf's entry as an explicit entry of its own, so
        /// rewriting the folder's list does not take it off: the removal pass
        /// has to, and this is what gap 2 of the review asked for.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn the_removal_takes_the_entry_off_every_existing_child() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("src")).unwrap();
            let child = root.path().join("src").join("main.rs");
            std::fs::write(&child, "fn main() {}").unwrap();
            grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            assert!(mentions(&child, &sid));
            revoke(root.path(), &[SID]).unwrap();
            assert!(!mentions(root.path(), &sid), "the folder keeps an entry");
            assert!(!mentions(&root.path().join("src"), &sid));
            assert!(!mentions(&child, &sid), "a child keeps an entry");
        }

        /// A removal propagates nothing of its own and refuses nothing: a
        /// junction planted after the grant is not entered, and its target
        /// keeps its list. This is the exposure of gap 2.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn the_removal_reaches_neither_a_junction_target_nor_a_hard_linked_file() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("owner.txt"), "x").unwrap();
            let store = tempfile::tempdir().unwrap();
            let stored = store.path().join("pkg.js");
            std::fs::write(&stored, "x").unwrap();
            std::fs::hard_link(&stored, root.path().join("pkg.js")).unwrap();
            grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            junction(&root.path().join("jn"), outside.path());
            // Not refused, whatever the tree holds now.
            revoke(root.path(), &[SID]).unwrap();
            assert!(!mentions(outside.path(), &sid));
            assert!(!mentions(&outside.path().join("owner.txt"), &sid));
            // The store file was never written, by the grant or the removal.
            assert!(!mentions(&stored, &sid));
        }

        /// The grant pass does not enter what kGraf denies, so no entry of
        /// it stands where the deny must hold.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn the_grant_pass_does_not_enter_a_denied_folder() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let gitdir = root.path().join(".git");
            std::fs::create_dir_all(&gitdir).unwrap();
            let config = gitdir.join("config");
            std::fs::write(&config, "[core]").unwrap();
            let plain = root.path().join("README.md");
            std::fs::write(&plain, "x").unwrap();
            let skipped =
                grant_writable_root(root.path(), &sid, &[], std::slice::from_ref(&gitdir), false)
                    .unwrap();
            assert_eq!(skipped.denied, 1);
            assert!(!mentions(&gitdir, &sid));
            assert!(!mentions(&config, &sid));
            assert!(mentions(&plain, &sid));
        }

        /// What the pass costs: the wall time of a first grant on a tree of
        /// the size named here. The kernel's own propagation opens each child
        /// too, so this replaces a cost rather than adding one, but only a
        /// Windows run says by how much. Search the output for `MEASURE`.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn measure_the_cost_of_a_first_grant_on_a_tree() {
            const FOLDERS: usize = 100;
            const FILES_PER_FOLDER: usize = 20;
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            for d in 0..FOLDERS {
                let dir = root.path().join(format!("d{d}"));
                std::fs::create_dir_all(&dir).unwrap();
                for f in 0..FILES_PER_FOLDER {
                    std::fs::write(dir.join(format!("f{f}.rs")), "x").unwrap();
                }
            }
            let started = std::time::Instant::now();
            let skipped = grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            let elapsed = started.elapsed();
            println!(
                "MEASURE first-grant-pass: {} folders, {} files, {} children written, {} ms",
                FOLDERS,
                FOLDERS * FILES_PER_FOLDER,
                skipped.written,
                elapsed.as_millis()
            );
        }

        /// What the guard above prevents, measured: whether writing an
        /// inheritable entry on a folder carries it through a junction onto
        /// the junction's target. The guard holds either way; this line tells
        /// the run's reader which. Search the output for `MEASURE`.
        /// Two measurements in one run, both of claims kGraf now rests on.
        ///
        /// First, whether the handle opened with `MAXIMUM_ALLOWED` really
        /// suppresses the propagation: a child that exists gets no entry
        /// from that write, only from kGraf's own pass. The line prints
        /// `suppressed` as false where Windows propagated anyway.
        ///
        /// Second, the old question, with the write Windows propagates
        /// (`SetNamedSecurityInfoW` by path): whether that propagation
        /// follows a junction onto its target. Every allowance of the link
        /// walk rests on the answer.
        #[test]
        fn measure_whether_propagation_follows_a_junction() {
            use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;

            let sid = Sid::from_string(SID).unwrap();
            // The suppressed write: one folder, one file in it, no pass.
            let quiet = tempfile::tempdir().unwrap();
            let child = quiet.path().join("child.txt");
            std::fs::write(&child, "x").unwrap();
            let dacl = read_dacl(quiet.path()).unwrap();
            apply(
                quiet.path(),
                &dacl,
                &[explicit(&sid, TREE_MASK, GRANT_ACCESS, TREE_INHERIT)],
            )
            .unwrap();
            println!(
                "MEASURE maximum-allowed-suppresses-propagation: suppressed {}",
                !mentions(&child, &sid)
            );

            // The propagating write, by path, as Windows makes it.
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("owner.txt"), "x").unwrap();
            junction(&root.path().join("jn"), outside.path());
            let dacl = read_dacl(root.path()).unwrap();
            let entries = [explicit(&sid, TREE_MASK, GRANT_ACCESS, TREE_INHERIT)];
            let mut new_acl: *mut ACL = ptr::null_mut();
            // SAFETY: a valid entry slice and a valid old list; the new list
            // is LocalAlloc'd and freed by `Local`.
            let code = unsafe {
                SetEntriesInAclW(
                    entries.len() as u32,
                    entries.as_ptr(),
                    dacl.acl,
                    &mut new_acl,
                )
            };
            assert_eq!(code, ERROR_SUCCESS, "SetEntriesInAclW");
            let _guard = Local(new_acl.cast());
            let name = wide(root.path().as_os_str());
            // SAFETY: `name` is NUL-terminated and `new_acl` is a valid list.
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
            assert_eq!(code, ERROR_SUCCESS, "SetNamedSecurityInfoW");
            println!(
                "MEASURE grant-propagation-follows-junction: target folder {}, file in it {}",
                mentions(outside.path(), &sid),
                mentions(&outside.path().join("owner.txt"), &sid)
            );
        }

        /// A tree grant for a sandbox user also limits what an owner of an
        /// object in the tree may do, and keeps kGraf able to write the list
        /// again. Without this, a slot user whose entries a revoke took away
        /// still owned the files it created there and could give itself its
        /// rights back from its next folder.
        #[test]
        fn a_tree_grant_limits_an_owner_and_keeps_kgraf_able_to_write_the_list() {
            let sid = Sid::from_string(SID).unwrap();
            let owner_rights = Sid::from_string(acl::OWNER_RIGHTS_SID).unwrap();
            let me = owner_sid().unwrap();
            let root = tempfile::tempdir().unwrap();
            grant_writable_root(root.path(), &sid, &[], &[], true).unwrap();
            // The entry an owner is limited to: reading the list, on the
            // folder and on everything below it.
            let dacl = read_dacl(root.path()).unwrap();
            for flags in [NO_INHERITANCE, TREE_INHERIT] {
                assert!(
                    has_ace(
                        &dacl,
                        ACCESS_ALLOWED_ACE_TYPE,
                        &owner_rights,
                        acl::READ_CONTROL_RIGHT,
                        flags
                    ),
                    "the owner-rights entry is missing with flags {flags:#x}"
                );
                // kGraf keeps, by name, the two rights ownership gave it.
                assert!(
                    has_ace(
                        &dacl,
                        ACCESS_ALLOWED_ACE_TYPE,
                        me,
                        acl::READ_CONTROL_RIGHT | acl::WRITE_DAC_RIGHT,
                        flags
                    ),
                    "kGraf's own entry is missing with flags {flags:#x}"
                );
            }
            // Idempotent: a second grant finds every entry and writes
            // nothing, so it never rewrites a large tree twice.
            grant_writable_root(root.path(), &sid, &[], &[], true).unwrap();
            // Not locked out: kGraf can still write the list of the tree it
            // owns, which is what its own entry above is for. A file created
            // after the grant inherits the same entries.
            let file = root.path().join("new.txt");
            std::fs::write(&file, "x").unwrap();
            let dacl = read_dacl(&file).unwrap();
            println!(
                "MEASURE owner-rights-inherited-by-a-new-file: {}",
                mentions(&file, &owner_rights)
            );
            apply(&file, &dacl, &[]).expect("kGraf can write a child's list");
            let dacl = read_dacl(root.path()).unwrap();
            apply(root.path(), &dacl, &[]).expect("kGraf can write the tree's list");
        }

        /// A handle on the folder `dir`, opened by this test without
        /// `FILE_SHARE_DELETE` and closed when dropped: the condition that
        /// makes an open with `MAXIMUM_ALLOWED` fail with os error 32
        /// (`ERROR_SHARING_VIOLATION`), as other programs' handles made it
        /// fail on `C:\` and `C:\Windows\Tasks`. Sharing is checked per open
        /// handle, not per process, so a handle in this process meets the
        /// same refusal as one in another. A child process's current folder
        /// is not used: on Windows Server 2025 a folder `ping.exe` had as its
        /// current folder still opened with `MAXIMUM_ALLOWED`, so the
        /// condition never formed.
        struct DeleteShareHolder(#[allow(dead_code)] OwnedHandle);

        impl DeleteShareHolder {
            /// Open the handle, then check that the condition holds: an open
            /// of `dir` with `MAXIMUM_ALLOWED` fails with a sharing
            /// violation. Without that the tests below would prove nothing.
            fn start(dir: &Path) -> Self {
                use super::super::ERROR_SHARING_VIOLATION;
                use windows_sys::Win32::Storage::FileSystem::{FILE_LIST_DIRECTORY, FILE_TRAVERSE};
                let name = wide(dir.as_os_str());
                // List and traverse, the access a folder's open handle
                // usually has; read and write shared, delete not.
                // SAFETY: `name` is NUL-terminated; the call returns a handle
                // or an invalid-handle value.
                let h = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        ptr::null(),
                        OPEN_EXISTING,
                        FILE_FLAG_BACKUP_SEMANTICS,
                        ptr::null_mut(),
                    )
                };
                assert!(
                    !h.is_null() && h != INVALID_HANDLE_VALUE,
                    "open {} without delete sharing: {}",
                    dir.display(),
                    std::io::Error::last_os_error()
                );
                // SAFETY: a valid handle this test now owns alone.
                let holder = Self(unsafe { OwnedHandle::from_raw_handle(h) });
                match open_maximum_allowed(dir) {
                    Err(ERROR_SHARING_VIOLATION) => holder,
                    Ok(_) => panic!(
                        "{} held without delete sharing still opens with MAXIMUM_ALLOWED; \
                         the condition under test is absent",
                        dir.display()
                    ),
                    Err(code) => panic!(
                        "{} held without delete sharing fails a MAXIMUM_ALLOWED open with \
                         {code}, not 32; the condition under test is absent",
                        dir.display()
                    ),
                }
            }
        }

        const INHERITED_ACE: u32 = 0x10;

        /// The lone write that replaces a refused `MAXIMUM_ALLOWED` open
        /// writes the folder's own list and nothing below it: an inheritable
        /// entry written on a held folder reaches no existing child. This
        /// is the property the `MAXIMUM_ALLOWED` handle gave, measured for
        /// the call that replaces it (`SetKernelObjectSecurity`, whose page
        /// does not say in so many words that it propagates nothing).
        /// Windows only; compile-checked on the development host.
        #[test]
        fn a_lone_write_on_a_held_folder_changes_no_child() {
            let sid = Sid::from_string(SID).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let child = dir.path().join("child.txt");
            std::fs::write(&child, "x").unwrap();
            let sub = dir.path().join("sub");
            std::fs::create_dir_all(&sub).unwrap();
            let _held = DeleteShareHolder::start(dir.path());
            assert_eq!(
                open_for_security(dir.path()).unwrap().write,
                super::super::ListWrite::KernelObjectAlone
            );
            let dacl = read_dacl(dir.path()).unwrap();
            apply(
                dir.path(),
                &dacl,
                &[explicit(&sid, TREE_MASK, GRANT_ACCESS, TREE_INHERIT)],
            )
            .expect("a held folder's list is written");
            assert!(mentions(dir.path(), &sid), "the folder's own entry");
            assert!(!mentions(&child, &sid), "the file below took the entry");
            assert!(!mentions(&sub, &sid), "the folder below took the entry");
        }

        /// A grant and a revoke on a worktree while its root and a folder of
        /// it are held open without delete sharing: both succeed,
        /// every object holds the entries the pass wrote, the held child
        /// folder holds exactly the pass's own entry (its lone write took no
        /// inherited copy in, and the root's write carried nothing down),
        /// and after the revoke no object names the user.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn a_grant_and_a_revoke_succeed_with_folders_held_without_delete_sharing() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let src = root.path().join("src");
            let deep = src.join("deep");
            std::fs::create_dir_all(&deep).unwrap();
            let main = src.join("main.rs");
            std::fs::write(&main, "fn main() {}").unwrap();
            let leaf = deep.join("leaf.txt");
            std::fs::write(&leaf, "x").unwrap();
            let _at_root = DeleteShareHolder::start(root.path());
            let _at_src = DeleteShareHolder::start(&src);
            let skipped = grant_writable_root(root.path(), &sid, &[], &[], false)
                .expect("a grant with held folders in the tree");
            assert_eq!(skipped.blocked, 0, "{:?}", skipped.first_blocked);
            for path in [root.path(), &src, &deep, &main, &leaf] {
                let own = entries_for(path, &sid)
                    .into_iter()
                    .filter(|(_, flags)| flags & INHERITED_ACE == 0)
                    .count();
                assert!(own > 0, "{} lacks the pass's entry", path.display());
            }
            assert_eq!(
                entries_for(&src, &sid),
                vec![(ACCESS_ALLOWED_ACE_TYPE, CHILD_INHERIT)],
                "the held folder holds the pass's entry alone"
            );
            let notes = revoke(root.path(), &[SID]).expect("a revoke with held folders");
            assert!(notes.is_empty(), "{notes:?}");
            for path in [root.path(), &src, &deep, &main, &leaf] {
                assert!(
                    !mentions(path, &sid),
                    "{} keeps an entry: {:?}",
                    path.display(),
                    entries_for(path, &sid)
                );
            }
        }

        /// A folder the grant wrote through `SetSecurityInfo` holds the
        /// pass's entry and an inherited copy of its parent's. When a process
        /// holds that folder at the revoke, its list is written alone, which
        /// takes nothing in from the parent, so the copy must be dropped by
        /// kGraf, or the user keeps its access there.
        /// Windows only; compile-checked on the development host.
        #[test]
        fn a_revoke_through_a_lone_write_drops_the_inherited_copy() {
            let sid = Sid::from_string(SID).unwrap();
            let root = tempfile::tempdir().unwrap();
            let src = root.path().join("src");
            std::fs::create_dir_all(&src).unwrap();
            grant_writable_root(root.path(), &sid, &[], &[], false).unwrap();
            let before = entries_for(&src, &sid);
            assert!(
                before.iter().any(|(_, flags)| flags & INHERITED_ACE != 0),
                "precondition: the folder holds an inherited copy: {before:?}"
            );
            let _held = DeleteShareHolder::start(&src);
            revoke(root.path(), &[SID]).expect("a revoke with a held folder");
            assert!(
                !mentions(&src, &sid),
                "the held folder keeps {:?}",
                entries_for(&src, &sid)
            );
            assert!(!mentions(root.path(), &sid));
        }
    }

    /// Deny `sid` every read and write beneath the hidden folder `dir`. A
    /// writable root beneath it keeps its own explicit grant, which precedes
    /// the inherited deny.
    fn deny_all(
        dir: &Path,
        sid: &Sid,
        allowed: &[PathBuf],
    ) -> Result<super::Skipped, SandboxError> {
        refuse_reparse(dir)?;
        let flags = CHILD_INHERIT;
        let present = |d: &Dacl| has_ace(d, ACCESS_DENIED_ACE_TYPE, sid, DENY_ALL_MASK, flags);
        if present(&read_dacl(dir)?) {
            return Ok(super::Skipped::default());
        }
        // The access-list lock is taken inside, around the folder's own
        // read-modify-write and around each child's, with the walks outside
        // it. Nothing is denied to the pass itself: a deny is what this write
        // carries, so it must reach every child of the hidden folder.
        apply_inheritable(
            dir,
            &[explicit(sid, DENY_ALL_MASK, DENY_ACCESS, flags)],
            &present,
            allowed,
            &[],
            &|handle, child, is_dir| {
                write_child_entry(
                    handle,
                    child,
                    is_dir,
                    sid,
                    DENY_ALL_MASK,
                    DENY_ACCESS,
                    ACCESS_DENIED_ACE_TYPE,
                )
            },
        )
        .map_err(|e| planted_or_refused(dir, e))
    }

    /// Remove every explicit entry for the SIDs `sids` from `path`
    /// (`kgraf sandbox remove`, and a slot that moves to another folder). A
    /// path that no longer exists is skipped; a reparse point is refused,
    /// never followed (decision 0083).
    ///
    /// **A removal propagates nothing and refuses nothing.** Writing a
    /// folder's list again used to carry the folder's remaining inheritable
    /// entries onto everything below it, which is the same exposure a grant
    /// has: the kernel's walk may enter a junction, and it writes the one
    /// descriptor a hard-linked file shares with every other name of it.
    /// The folder's own list is now written with that propagation suppressed
    /// ([`open_for_security`]) and the entries are taken off each child by
    /// the same pass the grant uses ([`propagate`]), which enters no reparse
    /// point and writes no hard-linked file.
    ///
    /// No link walk guards it, and none is needed: the pass only ever takes
    /// access away, so nothing of ours can land outside the tree. A removal
    /// must also never be refused, because a refusal would leave one user's
    /// write grant on a folder another user now works in. Its callers (the
    /// slot table and `kgraf sandbox remove`) know only the folder, so they
    /// could not build a set of allowed link targets anyway.
    ///
    /// The pass has to run: a child carries kGraf's entry as an explicit
    /// entry of its own ([`write_child_entry`]), not as an inherited copy
    /// that would disappear when the folder's list is rewritten.
    ///
    /// **A folder another program holds does not stop it.** A handle open
    /// on the folder, or one below it, without `FILE_SHARE_DELETE` used to
    /// make the folder's open fail with `ERROR_SHARING_VIOLATION`, and the
    /// whole removal with it. Such a folder is now written through a handle
    /// no open refuses, with a write that propagates nothing
    /// ([`open_for_security`]). A folder that still cannot be opened fails
    /// this call with its path, and `kgraf sandbox remove` names it, goes on
    /// with the other folders, and fails at the end.
    ///
    /// **What a removal leaves.** A hard-linked file is skipped, as it is
    /// during a grant, and recorded. After the grant pass of this version
    /// such a file never holds an entry of ours; a tree granted by an older
    /// kGraf, whose kernel propagation reached it, can still hold one, and
    /// the owner takes it off with `icacls` on the file itself.
    pub(crate) fn revoke(path: &Path, sids: &[&str]) -> Result<Vec<String>, SandboxError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
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
        // Under the access-list lock, as every other write of a list is: a
        // grant that read this list before the revoke would otherwise write
        // the revoked entries back, and nothing would ever remove them,
        // because the slot table no longer names this folder for that user.
        let _acl = lock_acls()?;
        let dacl = read_dacl(path)?;
        apply(path, &dacl, &entries)?;
        if !path.is_dir() {
            return Ok(Vec::new());
        }
        let skipped = propagate(path, &[], &|handle, child, _| {
            remove_child_entries(handle, child, &sids)
        })?;
        // A removal is never refused, so what it left is reported instead:
        // the caller that has an output path carries the note to the owner
        // (a slot that moves to another folder reports it with the command
        // that took the slot).
        Ok(skipped
            .note(path, "the removal of the sandbox user's access")
            .into_iter()
            .collect())
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
    /// are ordered before allows by `SetEntriesInAclW`. A deny entry on a
    /// folder is inheritable, so it goes through the link walk as a grant
    /// does ([`apply_inheritable`]): a deny carried through a junction would
    /// write the owner's own files outside the tree (decision 0083).
    fn deny_writes(
        path: &Path,
        sid: &Sid,
        allowed: &[PathBuf],
    ) -> Result<super::Skipped, SandboxError> {
        let flags = if path.is_dir() {
            CHILD_INHERIT
        } else {
            NO_INHERITANCE
        };
        let present = |d: &Dacl| has_ace(d, ACCESS_DENIED_ACE_TYPE, sid, DENY_WRITE_MASK, flags);
        if present(&read_dacl(path)?) {
            return Ok(super::Skipped::default());
        }
        let entries = [explicit(sid, DENY_WRITE_MASK, DENY_ACCESS, flags)];
        if path.is_dir() {
            // As in `deny_all`: the deny must reach every child of the
            // folder, so nothing is held back from the pass, and the
            // access-list lock is taken inside, once per list written.
            apply_inheritable(
                path,
                &entries,
                &present,
                allowed,
                &[],
                &|handle, child, is_dir| {
                    write_child_entry(
                        handle,
                        child,
                        is_dir,
                        sid,
                        DENY_WRITE_MASK,
                        DENY_ACCESS,
                        ACCESS_DENIED_ACE_TYPE,
                    )
                },
            )
            .map_err(|e| planted_or_refused(path, e))
        } else {
            // A file's entry is not inheritable, so no tree is walked, and
            // the lock is taken here, around this one list.
            let _acl = lock_acls()?;
            let dacl = read_dacl(path)?;
            if present(&dacl) {
                return Ok(super::Skipped::default());
            }
            apply(path, &dacl, &entries)?;
            Ok(super::Skipped::default())
        }
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
    ///
    /// The label is inheritable, so Windows writes it on everything already
    /// below the root, and it is written only while it is absent, as the
    /// grants are: it then goes through the link walk
    /// ([`apply_inheritable`]) and costs one read of the root's label on
    /// every later command instead of a write of the whole tree. A label
    /// carried through a junction would mark the owner's own folder Low,
    /// which every Low process could then write.
    fn label_root_low(root: &Path, allowed: &[PathBuf]) -> Result<super::Skipped, SandboxError> {
        let low = low_sid()?;
        if has_ace(
            &read_label(root)?,
            SYSTEM_MANDATORY_LABEL_ACE_TYPE,
            &low,
            SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
            CHILD_INHERIT,
        ) {
            return Ok(super::Skipped::default());
        }
        let inheritable = low_label_acl(&low, CHILD_INHERIT)?;
        let effective = low_label_acl(&low, NO_INHERITANCE)?;
        let write = || -> Result<super::Skipped, SandboxError> {
            let handle = open_for_security(root)?;
            write_label(&handle, root, &inheritable)?;
            // The label reaches every child, the folders kGraf denies the
            // sandbox user included: a Low label is not an access grant, and
            // the deny entry on such a folder is what holds there.
            propagate(root, &[], &|handle, path, is_dir| {
                let (acl, flags) = if is_dir {
                    (&inheritable, CHILD_INHERIT)
                } else {
                    (&effective, NO_INHERITANCE)
                };
                if has_ace(
                    &read_label_handle(&handle.handle, path)?,
                    SYSTEM_MANDATORY_LABEL_ACE_TYPE,
                    &low,
                    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                    flags,
                ) {
                    return Ok(());
                }
                write_label(handle, path, acl)
            })
        };
        super::guarded_inheritable_write(|| link_walk_refusal(root, allowed), write)
            .map_err(|e| planted_or_refused(root, e))
    }

    /// An access list holding one Low mandatory-label entry (`no-write-up`)
    /// with the inheritance flags `inherit`. The inheritance rules are the
    /// same for a label as for an access entry (Windows API, ACE Inheritance
    /// Rules: "These inheritance rules work the same for both DACLs and
    /// system access control lists"; read 2026-10-08), so a folder gets the
    /// inheritable form and a file the effective one.
    fn low_label_acl(low: &Sid, inherit: u32) -> Result<Vec<u32>, SandboxError> {
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
                inherit,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                low.as_psid(),
            )
        };
        if ok == 0 {
            return Err(last_error("AddMandatoryAce"));
        }
        Ok(acl)
    }

    /// Write `acl` as the mandatory label of whatever `handle` reached, with
    /// no propagation to existing children ([`open_for_security`]).
    ///
    /// Only through a `MAXIMUM_ALLOWED` handle. The lone write
    /// ([`super::ListWrite::KernelObjectAlone`]) is for access lists alone:
    /// its handle holds no `WRITE_OWNER`, which a label write needs, and
    /// `SetSecurityInfo` through it would propagate. Such an object fails
    /// with the sharing violation its first open met, as it did before the
    /// second open existed; the pass records it.
    fn write_label(handle: &ListHandle, path: &Path, acl: &[u32]) -> Result<(), SandboxError> {
        if handle.write != super::ListWrite::SetSecurityInfo {
            return Err(acl_error(path, super::ERROR_SHARING_VIOLATION));
        }
        // SAFETY: a valid handle and a valid label ACL.
        let code = unsafe {
            SetSecurityInfo(
                handle.handle.as_raw_handle(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                acl.as_ptr().cast(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        Ok(())
    }

    // ---- prepare --------------------------------------------------------------

    /// Capability SIDs for the policy's writable roots, whose ACLs are in place.
    #[derive(Debug)]
    pub(in crate::sandbox) struct Prepared {
        caps: Vec<Sid>,
        /// What the owner must know about this command's confinement, shown
        /// with its output
        /// ([`super::super::SandboxedCommand::refused_reads`]): a read root
        /// that is not granted, with the reason; the children a grant, a deny
        /// or the Low label could not write ([`super::Skipped::note`]); and
        /// what a slot moving to this folder left behind on the folders it
        /// held.
        pub(in crate::sandbox) notes: Vec<String>,
        /// Identifies the workspace, so its private desktop is reused.
        desktop_key: String,
        /// When set, the command starts as this sandbox user through the
        /// runner (decision 0097) instead of under the restricted token.
        user: Option<SandboxUser>,
        /// The `safe.directory` values git gets in the command
        /// ([`super::safe_directories`]): the roots, for the sandbox user;
        /// none for the restricted token, which runs as the owner.
        pub(in crate::sandbox) safe_directories: Vec<String>,
        /// The worktree hold of the policy (`SandboxPolicy::holding`): each
        /// command started with it is listed under it while its job holds a
        /// process ([`live_jobs_holding`]).
        pub(in crate::sandbox) hold: Option<PathBuf>,
    }

    /// The job of every command this process started with a worktree hold,
    /// by hold file. On Unix every process a command starts holds a shared
    /// lock on the hold file, which `WorkspaceManager::quiesce` cannot take
    /// while any of them runs. Windows has no such lock here, and before
    /// this list `quiesce` found nothing on Windows: a verifier committed a
    /// worktree a background process still wrote (a hosted Windows self-test,
    /// `a_worktree_the_worker_still_writes_is_not_snapshotted`, "expected a
    /// busy failure, got Pass").
    ///
    /// Weak references only: the job is closed, which ends its processes,
    /// when the [`SandboxedChild`] that owns it is dropped, and this list
    /// must never keep it open. A command started by another process is not
    /// listed: kGraf runs its workers' commands in its own process.
    static HELD_JOBS: std::sync::Mutex<Vec<(PathBuf, std::sync::Weak<OwnedHandle>)>> =
        std::sync::Mutex::new(Vec::new());

    /// List `job` under `hold`, when there is one, and return it shared.
    fn held_job(hold: Option<&Path>, job: OwnedHandle) -> Arc<OwnedHandle> {
        let job = Arc::new(job);
        if let Some(hold) = hold {
            let mut held = HELD_JOBS.lock().unwrap_or_else(|p| p.into_inner());
            // Entries whose job is closed are dropped here, so the list
            // stays as long as the commands that still run.
            held.retain(|(_, j)| j.strong_count() > 0);
            held.push((hold.to_path_buf(), Arc::downgrade(&job)));
        }
        job
    }

    /// How many commands this process started with the worktree hold `hold`
    /// still have a process in their job.
    pub(crate) fn live_jobs_holding(hold: &Path) -> usize {
        let jobs: Vec<Arc<OwnedHandle>> = {
            let mut held = HELD_JOBS.lock().unwrap_or_else(|p| p.into_inner());
            held.retain(|(_, j)| j.strong_count() > 0);
            held.iter()
                .filter(|(h, _)| h == hold)
                .filter_map(|(_, j)| j.upgrade())
                .collect()
        };
        // A job that cannot be read counts as live: a quiesce that is wrong
        // waits, one that is wrong the other way commits a tree in motion.
        jobs.iter()
            .filter(|job| job_processes(job).is_none_or(|n| n > 0))
            .count()
    }

    /// How many processes `job` holds, or `None` when that cannot be read.
    fn job_processes(job: &OwnedHandle) -> Option<u32> {
        // SAFETY: zeroed is a valid value for this plain C struct.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid job handle with query access, and a struct of the
        // stated size and class.
        let ok = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                ptr::addr_of_mut!(info).cast(),
                std::mem::size_of_val(&info) as u32,
                ptr::null_mut(),
            )
        };
        (ok != 0).then_some(info.ActiveProcesses)
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
        // A link beneath one writable root that leads into another is
        // allowed: this token is granted write on both. Reads are not
        // confined on this backend, so there is no read root to add
        // ([`super::allowed_link_targets`]).
        let granted = roots.clone();
        // The folders the grant pass must not enter: the top-level `.git` of
        // each writable root and the policy's hidden folders. Their deny
        // entries are written after the grants, and a plain allow entry on a
        // child of theirs would stand beside a deny the same pass writes.
        let denied: Vec<PathBuf> = roots
            .iter()
            .map(|root| root.join(".git"))
            .chain(policy.hidden.iter().map(|h| strip_verbatim(&h.dir)))
            .collect();
        let mut notes: Vec<String> = Vec::new();
        for (root, sid) in roots.iter().zip(&caps) {
            // Order matters: refuse a junction and a non-ACL volume before any
            // security call, so neither is followed or silently ineffective.
            refuse_reparse(root)?;
            require_acl_volume(root)?;
            notes.extend(
                grant_writable_root(root, sid, &granted, &denied, false)?
                    .note(root, "the write grant"),
            );
            notes.extend(label_root_low(root, &granted)?.note(root, "the Low integrity label"));
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
                notes
                    .extend(deny_writes(&gitlink, sid, &granted)?.note(&gitlink, "the write deny"));
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
                notes.extend(deny_writes(&dir, sid, &granted)?.note(&dir, "the write deny"));
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
            notes,
            desktop_key,
            user: None,
            safe_directories: Vec::new(),
            hold: None,
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
        //
        // Every root is judged before the lease, not inside the grant loop
        // below, for that reason: a root the lease recorded but that no grant
        // can be written on is a root no revoke can clear either, and it
        // would make every later folder that reached this slot fail. The
        // judgment needs no user, so it costs nothing to do first, the
        // closed-folder refusal included.
        //
        // The folders setup closed to the sandbox group. Read here, not once
        // per process: the drive set can change, and the cost (one registry
        // read per known folder and one listing of Public) is nothing beside
        // the command that follows.
        let closed = secure::locations()?;
        for root in &roots {
            // As in `prepare`: refuse a junction and a non-ACL volume before
            // any security call.
            refuse_reparse(root)?;
            require_acl_volume(root)?;
            refuse_closed_root(root, &closed)?;
        }
        // The lease may move a slot to this folder, which removes the slot
        // user's access from the folders it held; what that removal left
        // comes back as a note and goes out with this command's output.
        let (slot, mut notes) =
            users.lease(pool, policy.worker_slot.as_ref(), &roots, COMMAND_WAIT)?;
        let user = users.user(slot.role())?;
        let sid = Sid::from_string(&user.sid)?;
        let profile = std::env::var_os("USERPROFILE")
            .filter(|p| !p.is_empty())
            .and_then(|p| kgraf_core::plain_path::canonicalize(PathBuf::from(p)).ok());
        let (reads, env) = user_reads(policy, profile.as_deref());
        // Every folder this call admits this user to. A link that leads into
        // one of them does not refuse the grant: the user reaches it anyway
        // ([`super::allowed_link_targets`]). The dependency trees kGraf links
        // into a worktree are among the read roots, so a worktree that comes
        // back on another slot user is granted with its links in place.
        let granted: Vec<PathBuf> = roots.iter().chain(reads.iter()).cloned().collect();
        let dependency_trees: Vec<PathBuf> = policy
            .dependency_reads
            .iter()
            .map(|t| strip_verbatim(t))
            .collect();
        let checkout = policy
            .dependency_checkout
            .as_deref()
            .map(strip_verbatim)
            .filter(|_| !dependency_trees.is_empty());
        let allowed_for = |path: &Path| {
            super::allowed_link_targets(path, &granted, &dependency_trees, checkout.as_deref())
        };
        // The folders the grant pass must not enter, so no plain allow entry
        // of it lands where a deny is meant to hold. The denies themselves
        // are written below, and `SetEntriesInAclW` keeps a deny entry
        // before an allow one, so a deny wins on a child that holds both.
        let denied: Vec<PathBuf> = roots
            .iter()
            .map(|root| root.join(".git"))
            .chain(policy.hidden.iter().map(|h| strip_verbatim(&h.dir)))
            .collect();
        for root in &roots {
            notes.extend(
                grant_writable_root(root, &sid, &allowed_for(root), &denied, true)?
                    .note(root, "the write grant"),
            );
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
            notes.extend(
                deny_writes(&gitlink, &sid, &allowed_for(&gitlink))?
                    .note(&gitlink, "the write deny"),
            );
        }
        // A hidden folder always lies beneath a writable root (the policy
        // checks it), so the sandbox user is denied there.
        for hidden in &policy.hidden {
            let dir = strip_verbatim(&hidden.dir);
            notes.extend(deny_all(&dir, &sid, &allowed_for(&dir))?.note(&dir, "the deny"));
        }
        // Every write grant and every deny entry of this command is written
        // by now, so this reads the state the command will meet. A grant that
        // reads back is not proof: a deny entry is evaluated before any
        // grant. The read grants that follow add no entry on a writable root.
        for root in &roots {
            verify_effective_write_as_user(root, user)?;
        }
        for dir in &reads {
            // A read root inside a writable root is already granted there.
            if roots.iter().any(|r| dir.starts_with(r)) {
                continue;
            }
            match grant_read(dir, &sid, &allowed_for(dir))? {
                ReadGrant::Written(skipped) => {
                    notes.extend(skipped.note(dir, "the read grant"));
                    users.record_grant(dir)?;
                }
                ReadGrant::Present => {}
                // Not granted, so the command cannot read it; it runs and
                // the reason goes out with its output
                // (`SandboxedCommand::refused_reads`).
                ReadGrant::Refused(why) => notes.push(super::refused_read_note(dir, &why)),
            }
        }
        // The runner must be readable and runnable by every sandbox user,
        // wherever kGraf is installed. It is a file, so its entry is not
        // inheritable and no tree is walked for it.
        for u in users.all() {
            match grant_read(&users.runner, &Sid::from_string(&u.sid)?, &[])? {
                ReadGrant::Written(_) => users.record_grant(&users.runner)?,
                ReadGrant::Present => {}
                // A file is never walked, so this cannot be reached; the
                // runner must be readable, so it fails loud.
                ReadGrant::Refused(why) => {
                    return Err(SandboxError::PlatformSetup {
                        step: "grant the runner to the sandbox users",
                        source: io::Error::other(why),
                    });
                }
            }
        }
        let readable: Vec<PathBuf> = policy.readable.iter().map(|r| strip_verbatim(r)).collect();
        Ok(Prepared {
            caps: Vec::new(),
            notes,
            desktop_key: String::new(),
            safe_directories: super::safe_directories(&roots, &readable),
            hold: None,
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
        // A note of what a removal left is dropped here: this call only
        // takes the slot for a worker, with no output path of its own, and
        // the first command in the folder reports the same files
        // (`prepare_for_user`).
        let (slot, _notes) = users.lease(
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
    ///
    /// `dependency_trees` are the checkout's trees the caller is about to
    /// link into `root`. A link in the root that leads into one of them does
    /// not refuse the grant: a tree that is linked here is granted to the
    /// same user, so the propagation would reach nothing new, and the links
    /// are kGraf's own, so refusing them would refuse the folder for good
    /// (the advice "remove the link" is wrong: kGraf makes it again). The
    /// checkout root itself is not in the set: a command may write in this
    /// folder and could plant a junction to the checkout root here.
    pub(in crate::sandbox) fn prepare_root(
        root: &Path,
        dependency_trees: &[PathBuf],
    ) -> Result<Vec<String>, SandboxError> {
        let root = strip_verbatim(root);
        let allowed: Vec<PathBuf> = dependency_trees.iter().map(|t| strip_verbatim(t)).collect();
        // The grant pass leaves the root's own `.git` alone, as `prepare`
        // does: the deny on it is written when the first command runs here,
        // and a plain allow entry below it would stand beside that deny.
        let denied = vec![root.join(".git")];
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
            let (slot, mut notes) = users.lease(
                Pool::Offline,
                None,
                std::slice::from_ref(&root),
                COMMAND_WAIT,
            )?;
            let user = users.user(slot.role())?;
            notes.extend(
                grant_writable_root(
                    &root,
                    &Sid::from_string(&user.sid)?,
                    &allowed,
                    &denied,
                    true,
                )?
                .note(&root, "the write grant"),
            );
            return Ok(notes);
        }
        let sid = Sid::from_string(&capability_sid_string(&root.to_string_lossy()))?;
        refuse_reparse(&root)?;
        require_acl_volume(&root)?;
        let mut notes: Vec<String> = Vec::new();
        notes.extend(
            grant_writable_root(&root, &sid, &allowed, &denied, false)?
                .note(&root, "the write grant"),
        );
        notes.extend(label_root_low(&root, &allowed)?.note(&root, "the Low integrity label"));
        verify_effective_write(&root, &sid)?;
        Ok(notes)
    }

    /// Refuse a writable root that setup closed to the sandbox group.
    ///
    /// A deny entry is evaluated before any grant, so a root equal to one of
    /// those folders takes the grant, reads it back as present, and then
    /// fails every write at runtime with nothing naming the cause.
    fn refuse_closed_root(root: &Path, closed: &Locations) -> Result<(), SandboxError> {
        match closed_folder_of(root, closed) {
            None => Ok(()),
            Some(folder) => Err(SandboxError::PlatformSetup {
                step: "check a writable root against the folders closed to the sandbox users",
                source: io::Error::other(format!(
                    "{} is {}, where `kgraf sandbox setup` denies the sandbox users the right                      to create or change entries, and a deny is evaluated before any grant, so                      no write there could succeed. Give the command a folder of its own.",
                    root.display(),
                    folder.display()
                )),
            }),
        }
    }

    /// Confirm the sandbox user is really granted write on `root`, under its
    /// own token, after every grant and deny of this command is written.
    ///
    /// [`verify_effective_write`] does not serve here: it builds a
    /// restricted token from this process's own, which carries the owner's
    /// identity, and a deny entry for the sandbox group would not be
    /// evaluated in it at all. This logs the sandbox user on instead, which
    /// is one more logon per command beside the one the runner already does.
    fn verify_effective_write_as_user(root: &Path, user: &LoadedUser) -> Result<(), SandboxError> {
        let token = accounts::logon_token(&user.name, user.password.expose()).map_err(|why| {
            SandboxError::PlatformSetup {
                step: "log the sandbox user on to check its write access",
                source: io::Error::other(why),
            }
        })?;
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
            return Err(last_error("DuplicateTokenEx(AccessCheck, sandbox user)"));
        }
        let imp = owned(imp, "DuplicateTokenEx")?;
        if !effective_write_granted(root, &imp)? {
            return Err(SandboxError::PlatformSetup {
                step: "verify effective write access as the sandbox user",
                source: io::Error::other(format!(
                    "the sandbox user {} is not granted write on {}, although its allow entry                      is present: a deny entry on that folder outranks it",
                    user.name,
                    root.display()
                )),
            });
        }
        Ok(())
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
        /// The relay thread of an inherited console stream, if any: its
        /// channel closes when the thread ends (see [`RelayDone`]).
        relay: Option<RelayDone>,
    }

    /// Closed (disconnected) when a console relay thread has copied the last
    /// byte and ended.
    type RelayDone = std::sync::mpsc::Receiver<()>;

    /// A copy of `h` this process owns, not inheritable (see [`stream`]).
    fn duplicate(h: HANDLE, step: &'static str) -> Result<OwnedHandle, SandboxError> {
        let mut dup: HANDLE = ptr::null_mut();
        // SAFETY: duplicates a valid handle within this process.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h,
                GetCurrentProcess(),
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(last_error(step));
        }
        owned(dup, step)
    }

    /// Whether `h` is a console or another character device. A console
    /// handle answers `GetConsoleMode`; the device type also catches one
    /// opened without the read access that call needs.
    fn is_console_or_device(h: HANDLE) -> bool {
        let mut mode = 0;
        // SAFETY: probes a valid handle; neither call changes it.
        unsafe { GetConsoleMode(h, &mut mode) != 0 || GetFileType(h) == FILE_TYPE_CHAR }
    }

    /// The refusal of an inherited standard input that is a console: a
    /// handle given to a child is admitted with no access check, and
    /// `WriteConsoleInput` through it injects keystrokes into the owner's
    /// console. A command never gets an interactive console.
    const CONSOLE_INPUT_REFUSED: &str = "an inherited console input handle or other character \
                                         device as standard input (WriteConsoleInput through a \
                                         console injects keystrokes into the user's console); \
                                         use Redirect::Null or Redirect::Piped";

    /// One standard stream of a command. Every handle made here is **not
    /// inheritable**: kGraf's other spawns (std's, which pass every
    /// inheritable handle of this process to their child) must not copy the
    /// command's pipe ends, or the command's reader would wait for that
    /// unrelated process. The handles are marked inheritable only for the one
    /// creation call that passes them, under the process-wide spawn lock
    /// ([`InheritWindow`]).
    fn stream(which: STD_HANDLE, redirect: Redirect) -> Result<Stream, SandboxError> {
        match redirect {
            Redirect::Null => {
                let sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: ptr::null_mut(),
                    bInheritHandle: 0,
                };
                let nul = wide(OsStr::new("NUL"));
                // SAFETY: NUL-terminated device name, handle not inheritable.
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
                    relay: None,
                })
            }
            Redirect::Piped => {
                // Both ends are made without inheritance (`child_pipe`).
                let (parent, child) = child_pipe(which != STD_INPUT_HANDLE)?;
                Ok(Stream {
                    child: Some(child),
                    parent: Some(parent),
                    relay: None,
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
                        relay: None,
                    });
                }
                if which == STD_INPUT_HANDLE && is_console_or_device(h) {
                    return Err(SandboxError::PolicyNotEnforceable {
                        backend: BACKEND_NAME,
                        what: CONSOLE_INPUT_REFUSED,
                    });
                }
                Ok(Stream {
                    child: Some(duplicate(h, "DuplicateHandle(std handle)")?),
                    parent: None,
                    relay: None,
                })
            }
        }
    }

    /// Handles marked inheritable for one process creation call. Taken under
    /// the process-wide spawn lock (`kgraf_core::child_run::spawn_lock`),
    /// which `kgraf_core::child_run::run` also takes around its spawn, and
    /// cleared before the lock is released ([`Self::close`], or on drop on an
    /// error path).
    ///
    /// The creation calls need the flag: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
    /// accepts only inheritable handles, and `CreateProcessWithLogonW`'s
    /// handling of non-inheritable standard handles is not documented. The
    /// handle list keeps the sandboxed command from inheriting anything else;
    /// the lock keeps every other spawn from inheriting these. Every spawn in
    /// kGraf takes it (`kgraf_core::child_run::spawn_locked`); a test that
    /// reads every crate's source keeps it that way. A process another
    /// library starts on its own (none known) would not.
    pub(crate) struct InheritWindow {
        marked: Vec<HANDLE>,
        // Declared last: released after `Drop::drop` has cleared the flags.
        _lock: kgraf_core::child_run::SpawnLock,
    }

    impl InheritWindow {
        /// Take the spawn lock, then mark each non-null handle inheritable.
        pub(crate) fn open(handles: &[HANDLE]) -> Result<Self, SandboxError> {
            let lock = kgraf_core::child_run::spawn_lock(kgraf_core::child_run::SPAWN_LOCK_WAIT)
                .map_err(|timeout| {
                    SandboxError::Spawn(io::Error::new(io::ErrorKind::TimedOut, timeout))
                })?;
            let mut window = Self {
                marked: Vec::with_capacity(handles.len()),
                _lock: lock,
            };
            for &h in handles.iter().filter(|h| !h.is_null()) {
                // SAFETY: a valid handle the caller owns.
                if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0
                {
                    // Drop clears the handles marked so far.
                    return Err(last_error("mark a standard handle inheritable"));
                }
                window.marked.push(h);
            }
            Ok(window)
        }

        /// Clear the flag on every marked handle, then release the lock. An
        /// error names the step; the caller ends what it created, since a
        /// handle left inheritable could pass to an unrelated child.
        pub(crate) fn close(mut self) -> Result<(), SandboxError> {
            let mut first = None;
            for h in std::mem::take(&mut self.marked) {
                // SAFETY: a valid handle the caller owns.
                if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) } == 0
                    && first.is_none()
                {
                    first = Some(last_error("clear a standard handle's inheritance"));
                }
            }
            first.map_or(Ok(()), Err)
        }
    }

    impl Drop for InheritWindow {
        fn drop(&mut self) {
            for h in self.marked.drain(..) {
                // SAFETY: a valid handle the caller owns. Reached only on an
                // error path; the caller closes the handle right after.
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
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
    /// over everything else (a POSIX shell's [`SHELL_COMMAND_VAR`], the
    /// sandbox user's jobserver). With
    /// `inherit_all` false (the command's environment was cleared) only
    /// [`super::super::WINDOWS_BASE_VARS`] come from this process; `PATH` is one of
    /// them, so the lookup below stays right.
    fn final_env(
        cmd: &Command,
        extra: &[(&str, &str)],
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
        for (name, value) in extra {
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
        job: Arc<OwnedHandle>,
        /// The console relay threads of the command's inherited standard
        /// streams, waited for (bounded) when the child is dropped.
        relays: Vec<RelayDone>,
        /// See [`Self::runner_lists_before_resume`].
        runner_protection: Option<Result<(), String>>,
        /// Keeps the (shared, per-workspace) desktop alive while the child
        /// runs. `None` for a command that runs as the sandbox user: this
        /// process keeps that user's window station and desktop for its whole
        /// life (`sandbox_user::windows::launch`).
        _desktop: Option<DesktopGuard>,
        /// The jobserver semaphore of a command that runs as a sandbox user
        /// (`sandbox_user::jobserver`), kept while the command runs.
        _jobserver: Option<Jobserver>,
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

        /// For a command that runs as the sandbox user: whether kGraf wrote
        /// the protected access lists on its runner's process and main
        /// thread while the runner was still suspended, and read them back.
        /// `Some(Err)` names the step and the Win32 error: the runner then
        /// protected itself as its first step, and a program of the same
        /// sandbox user could open it until then. `None` on the token path,
        /// which has no runner.
        pub fn runner_lists_before_resume(&self) -> Option<Result<(), &str>> {
            self.runner_protection
                .as_ref()
                .map(|r| r.as_ref().map(|_| ()).map_err(String::as_str))
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

        /// The exit status once the process has exited, `None` if it still
        /// runs after `bound`. Does not close stdin (a caller that polls may
        /// still write to it) and ends nothing.
        pub fn wait_timeout(
            &mut self,
            bound: std::time::Duration,
        ) -> io::Result<Option<ExitStatus>> {
            let deadline = kgraf_core::waits::deadline_after(bound);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                // Below INFINITE (u32::MAX ms), which would never return; a
                // longer bound waits in several steps.
                let ms = u32::try_from(left.as_millis())
                    .unwrap_or(INFINITE - 1)
                    .min(INFINITE - 1);
                // SAFETY: a valid process handle.
                match unsafe { WaitForSingleObject(self.process.as_raw_handle(), ms) } {
                    WAIT_OBJECT_0 => return self.exit_status().map(Some),
                    WAIT_TIMEOUT if left.is_zero() || ms == 0 => return Ok(None),
                    WAIT_TIMEOUT => {}
                    _ => return Err(io::Error::last_os_error()),
                }
            }
        }

        /// What this command is called in a wait's error.
        fn wait_name(&self) -> String {
            format!("the sandboxed command (process {})", self.pid)
        }

        /// Wait for the process to exit, at most [`WAIT_BOUND`]. Closes stdin
        /// first, as std does. See [`Self::wait_within`].
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.wait_within(WAIT_BOUND)
        }

        /// Wait for the process to exit, at most `bound`. Closes stdin first,
        /// as std does. At the bound the whole tree is ended and the result
        /// is a [`io::ErrorKind::TimedOut`] error naming the process and the
        /// bound.
        pub fn wait_within(&mut self, bound: std::time::Duration) -> io::Result<ExitStatus> {
            drop(self.stdin.take());
            let what = self.wait_name();
            crate::sandbox::bounded_wait::wait_exit(
                self,
                &what,
                crate::sandbox::bounded_wait::Bounds::new(bound),
            )
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
            kgraf_core::child_run::end_job(self, bound)
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

        /// [`Self::wait_with_output_within`] with [`WAIT_BOUND`].
        pub fn wait_with_output(self) -> io::Result<Output> {
            self.wait_with_output_within(WAIT_BOUND)
        }

        /// Collect stdout and stderr, wait for the exit at most `bound`, end
        /// what the command left running, and return what it wrote. Every
        /// step is bounded (`crate::sandbox::bounded_wait`): a command still
        /// running at `bound` is ended with its tree and is a
        /// [`io::ErrorKind::TimedOut`] error; a pipe a process outside the
        /// job keeps open does not hold the call past
        /// [`kgraf_core::child_run::DRAIN_GRACE`], and the case is recorded
        /// (`kgraf_core::waits::pipes_held_count`).
        pub fn wait_with_output_within(mut self, bound: std::time::Duration) -> io::Result<Output> {
            drop(self.stdin.take());
            let what = self.wait_name();
            let stdout = self
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>);
            let stderr = self
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>);
            let got = crate::sandbox::bounded_wait::collect(
                &mut self,
                stdout,
                stderr,
                &what,
                crate::sandbox::bounded_wait::Bounds::new(bound),
            )?;
            Ok(Output {
                status: got.status,
                stdout: got.stdout,
                stderr: got.stderr,
            })
        }
    }

    impl kgraf_core::child_run::Job for SandboxedChild {
        fn terminate(&mut self) -> io::Result<()> {
            self.kill()
        }
        fn active(&self) -> io::Result<u32> {
            self.active_processes()
        }
    }

    /// Closing the job handle only starts the kill: the processes end a
    /// moment later. The command's slot (and desktop) are released after
    /// this body, as the fields drop, so the processes are ended first, at
    /// most [`kgraf_core::child_run::KILL_BOUND`] (the longest a killed
    /// process has to end). Otherwise the next command could take the same
    /// sandbox user while a process of the old command still holds files in
    /// the folder. An error or a process that outlives the bound cannot be
    /// handled here; the slot is released all the same, and the job handle's
    /// close still ends what is left.
    impl Drop for SandboxedChild {
        fn drop(&mut self) {
            let _ = self.kill_tree_and_wait(kgraf_core::child_run::KILL_BOUND);
            // The relay threads of inherited console streams end when the
            // tree's last copy of their pipe closes, which the wait above
            // has seen. Waiting for them keeps the last bytes from being cut
            // when this process exits next. The bound is the drain grace
            // (microseconds when the pipe is closed; the bytes are already
            // in it): a pipe a process outside the tree holds open ends the
            // wait, not the thread. No lock is held here.
            let end = kgraf_core::waits::deadline_after(kgraf_core::child_run::DRAIN_GRACE);
            for done in self.relays.drain(..) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let _ = done.recv_timeout(left);
            }
        }
    }

    impl crate::sandbox::bounded_wait::Exits for SandboxedChild {
        type Status = ExitStatus;
        fn wait_timeout(&mut self, bound: std::time::Duration) -> io::Result<Option<ExitStatus>> {
            SandboxedChild::wait_timeout(self, bound)
        }
        fn end_tree(&mut self, bound: std::time::Duration) -> io::Result<bool> {
            self.kill_tree_and_wait(bound)
        }
    }

    /// The bound of [`SandboxedChild::wait`], [`SandboxedChild::wait_with_output`]
    /// and the `status` and `output` of a sandboxed command on Windows, which
    /// take no bound of their own. Their longest legitimate use is a person's
    /// command through `kgraf-wintest sandbox-run`, such as a cold build and
    /// test of a large workspace on a slow machine, which runs for most of an
    /// hour; two hours leave room for that, so a wait this long is a command
    /// that does not end. A caller with a bound of its own uses
    /// [`SandboxedChild::wait_within`] or
    /// [`SandboxedChild::wait_with_output_within`].
    pub const WAIT_BOUND: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

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
        // A command that runs as the sandbox user cannot create a named
        // object in the session, so cargo and rustc could not make their
        // jobserver: this process makes one the user may use, and names it
        // in the variable they read first (`sandbox_user::jobserver`). Set
        // over the command's own value and an inherited one: a jobserver of
        // this process's own cargo admits this process's session, not the
        // sandbox user.
        let jobserver = match &prepared.user {
            Some(user) => Some(jobserver::create(
                &accounts::current_user_sid()?,
                &user.user.sid,
            )?),
            None => None,
        };
        let mut extra: Vec<(&str, &str)> = Vec::new();
        if let Some(l) = &launch {
            extra.push((SHELL_COMMAND_VAR, l.command.as_str()));
        }
        if let Some(j) = &jobserver {
            extra.push((JOBSERVER_VAR, j.makeflags.as_str()));
        }
        let (env, path) = final_env(
            cmd,
            &extra,
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
            return spawn_as_user(
                user,
                &program,
                line,
                cwd,
                env,
                stdio,
                UserChildParts {
                    jobserver,
                    hold: prepared.hold.as_deref(),
                },
            );
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
        // handle list present, and FALSE otherwise. The listed handles must be
        // marked inheritable, which they are only for the creation call below,
        // under the spawn lock (`InheritWindow`).
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
        let window = InheritWindow::open(&[raw(&stdin), raw(&stdout), raw(&stderr)])?;
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
            let err = io::Error::last_os_error();
            drop(window);
            return Err(SandboxError::Spawn(err));
        }
        let process = owned(pi.hProcess, "CreateProcessAsUserW process")?;
        let thread = owned(pi.hThread, "CreateProcessAsUserW thread")?;
        if let Err(e) = window.close() {
            // SAFETY: the suspended process never ran; terminate it.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(e);
        }
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
            job: held_job(prepared.hold.as_deref(), job),
            relays: Vec::new(),
            runner_protection: None,
            _desktop: Some(DesktopGuard(desktop)),
            _jobserver: None,
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

    /// A stream for a command that runs as the sandbox user. No handle to
    /// the owner's console is ever given to the command:
    ///
    /// - Standard input: [`Redirect::Null`] (the default) is the NUL device,
    ///   [`Redirect::Piped`] a pipe, as on the token path. An inherited
    ///   standard input that is a console, or any other character device, is
    ///   refused (a command never gets an interactive console); one that is a
    ///   pipe or a file is passed as a copy.
    /// - Standard output and error inherited from a console: the command
    ///   writes to a pipe, and a thread of this process copies what arrives to
    ///   this process's own stream.
    ///
    /// A console handle carries more than writing text. Its holder can read
    /// what the window shows and add to its input, and a handle passed to a
    /// child is admitted without any access check, so neither the sandbox
    /// user's own list nor the deny-only logon SID would stop it.
    fn user_stream(which: STD_HANDLE, redirect: Redirect) -> Result<Stream, SandboxError> {
        if which == STD_INPUT_HANDLE && redirect == Redirect::Inherit {
            // SAFETY: plain call; the handle is borrowed, not owned.
            let h = unsafe { GetStdHandle(which) };
            if !h.is_null() && h != INVALID_HANDLE_VALUE && is_console_or_device(h) {
                return Err(SandboxError::PolicyNotEnforceable {
                    backend: USER_BACKEND_NAME,
                    what: CONSOLE_INPUT_REFUSED,
                });
            }
        }
        if redirect != Redirect::Inherit || which == STD_INPUT_HANDLE || !is_console(which) {
            return stream(which, redirect);
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            // Neither end is inheritable: the writing end is marked so only
            // for the runner's creation call (`InheritWindow`).
            bInheritHandle: 0,
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
        let mut from = std::fs::File::from(read);
        // The sender is never used: dropping it, when the thread ends, is the
        // signal. `SandboxedChild` waits for it (bounded) when it is dropped,
        // so the last bytes are copied before the caller goes on or exits.
        let (ended, done) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ended = ended;
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
            relay: Some(done),
        })
    }

    /// What a command that runs as the sandbox user keeps beside its process:
    /// the jobserver made for it, and the worktree hold its job is listed
    /// under ([`held_job`]).
    struct UserChildParts<'a> {
        jobserver: Option<Jobserver>,
        hold: Option<&'a Path>,
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
        parts: UserChildParts<'_>,
    ) -> Result<SandboxedChild, SandboxError> {
        let request = Request {
            application: program.as_os_str().encode_wide().collect(),
            command_line: line,
            directory: strip_verbatim(cwd).as_os_str().encode_wide().collect(),
            environment: env,
            // Named by `launch::start`: the desktop this process made for the
            // command of this sandbox user.
            desktop: Vec::new(),
        };
        // No UI limits. Windows nests a job inside another only if neither
        // sets them, and the runner may already be in a job (the secondary
        // logon service's, or one kGraf itself runs in). The runner and the
        // command stay in this job: kill on close, no breakaway. The
        // command's UI isolation is its own window station and desktop,
        // which this process makes (`sandbox_user::windows::launch`).
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
            request,
        )?;
        Ok(SandboxedChild {
            stdin: stdin.parent.map(ChildStdin::from),
            stdout: stdout.parent.map(ChildStdout::from),
            stderr: stderr.parent.map(ChildStderr::from),
            process: started.runner,
            pid: started.command_pid,
            job: held_job(parts.hold, job),
            relays: [stdout.relay, stderr.relay].into_iter().flatten().collect(),
            runner_protection: Some(started.protected_before_resume),
            _desktop: None,
            _jobserver: parts.jobserver,
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
        let (entries, count) = super::super::git_config_entries(&git_config_pairs(true, &[]));
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
            tool_env(true, &[]),
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
        let (entries, count) = super::super::git_config_entries(&git_config_pairs(false, &[]));
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
        let (entries, _) = super::super::git_config_entries(&git_config_pairs(true, &[]));
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
        let open = tool_env(true, &[]);
        assert_eq!(lookup(&open, "GIT_CONFIG_KEY_0"), Some("credential.helper"));
        assert_eq!(lookup(&open, "GIT_CONFIG_KEY_1"), Some("http.sslBackend"));
        assert_eq!(lookup(&open, "GIT_CONFIG_COUNT"), Some("5"));
        assert_eq!(lookup(&open, CURL_SSL_BACKEND_VAR), Some(TLS_BACKEND));
        let closed = tool_env(false, &[]);
        assert_eq!(lookup(&closed, "GIT_CONFIG_COUNT"), Some("4"));
        assert_eq!(lookup(&closed, CURL_SSL_BACKEND_VAR), None);
    }

    /// A command that runs as the sandbox user trusts, through git's
    /// command-scope configuration, each writable root and every repository
    /// below it, and each read root alone, in the form git compares; the
    /// entries follow kGraf's others, so the count covers them, and they
    /// survive a clear. Run 11 found `git status` refused with "detected
    /// dubious ownership" in the worker's worktree without them.
    #[test]
    fn the_sandbox_user_gets_safe_directory_entries_for_its_roots() {
        let writable = [
            std::path::PathBuf::from(r"C:\work\repo\.kgraf\workspaces\w1-1"),
            std::path::PathBuf::from(r"C:\work\repo\.kgraf\.scratch\w1-1\"),
        ];
        let readable = [std::path::PathBuf::from(r"C:\work\repo\.git")];
        let safe = safe_directories(&writable, &readable);
        assert_eq!(
            safe,
            [
                "C:/work/repo/.kgraf/workspaces/w1-1",
                "C:/work/repo/.kgraf/workspaces/w1-1/*",
                "C:/work/repo/.kgraf/.scratch/w1-1",
                "C:/work/repo/.kgraf/.scratch/w1-1/*",
                "C:/work/repo/.git",
            ]
        );
        // A read root is trusted alone, never what lies below it.
        assert!(!safe.iter().any(|v| v.starts_with("C:/work/repo/.git/")));
        // No backslash and no wildcard for everything.
        assert!(safe.iter().all(|v| !v.contains('\\') && v != "*"));
        let env = tool_env(false, &safe);
        assert_eq!(lookup(&env, "GIT_CONFIG_COUNT"), Some("9"));
        assert_eq!(lookup(&env, "GIT_CONFIG_KEY_4"), Some("safe.directory"));
        assert_eq!(
            lookup(&env, "GIT_CONFIG_VALUE_4"),
            Some("C:/work/repo/.kgraf/workspaces/w1-1")
        );
        assert_eq!(
            lookup(&env, "GIT_CONFIG_VALUE_8"),
            Some("C:/work/repo/.git")
        );
        // The same entries after a clear.
        let kept: Vec<(String, String)> = kept_on_clear(false, None, &safe)
            .into_iter()
            .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(lookup(&kept, "GIT_CONFIG_COUNT"), Some("9"));
        assert_eq!(lookup(&kept, "GIT_CONFIG_KEY_8"), Some("safe.directory"));
        // The token sandbox, which runs as the owner, gets none.
        assert!(
            !tool_env(false, &[])
                .iter()
                .any(|(_, v)| v == "safe.directory")
        );
    }

    /// After a clear the git entries start at zero, as without one, the
    /// TLS settings survive, and `TMP` and `TEMP` are the policy's folder.
    #[test]
    fn a_clear_keeps_the_sandbox_settings_numbered_from_zero() {
        let tmp = std::path::Path::new(r"C:\ws\scratch");
        let kept: Vec<(String, String)> = kept_on_clear(true, Some(tmp), &[])
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
        let bare: Vec<String> = kept_on_clear(false, None, &[])
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
