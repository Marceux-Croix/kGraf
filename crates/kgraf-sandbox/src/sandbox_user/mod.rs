//! The Windows sandbox user (decision 0097): a one-time administrator setup
//! that creates dedicated local users for sandboxed commands, blocks the
//! offline user's network with firewall rules, and hands the users'
//! passwords to the owner's own process.
//!
//! A command that runs as another user, with that user's own token, has no
//! restricting SIDs. Restricting SIDs are what break Schannel TLS and the
//! MSYS2 runtime in the token sandbox (decision 0011, amendments of
//! 2026-10-07). Writes are confined by ACLs that grant the sandbox users
//! write access only where a command may write.
//!
//! ```text
//!   kgraf sandbox setup (the user, unelevated)
//!     | ShellExecuteExW "runas": kgraf sandbox elevated --action setup
//!     |   --pipe <name> --nonce <hex> --owner-pid <pid> --owner-sid <sid>
//!     v
//!   elevated process            named pipe (DACL: owner SID only;
//!     create pipe  <------------ client PID checked against --owner-pid,
//!     check owner token SID      server PID checked against the child)
//!     plan from current state
//!     run each step --- "step <key> <outcome>" ---> owner prints progress
//!     write undo record (ProgramData, admin-only writes)
//!     --- "password <role> <pw>" ---> owner stores DPAPI(CurrentUser) blob
//!     --- "end" --->
//! ```
//!
//! Everything in this module but `windows` is platform-neutral, so the
//! plan, the names, the rule specifications and the formats are tested on
//! every host. The Windows calls live in `windows` and are thin.
//!
//! **Reuse.** The design follows codex's elevated Windows sandbox
//! (github.com/openai/codex, `codex-rs/windows-sandbox-rs`, Apache-2.0, read
//! at commit 9b738582b13c2cdbeff54af0afd04c50c3e7ba09): an offline and an
//! online user, a password reset on every setup, hiding the users from the
//! sign-in screen, firewall rules scoped by `LocalUserAuthorizedList` to the
//! offline user's SID, its non-loopback address literals, its loopback
//! blocks with the proxy ports cut out, and its local policy check. See
//! `third_party/codex/UPSTREAM` for what is taken and what differs.

pub mod blob;
pub mod firewall;
pub mod handshake;
pub mod names;
pub mod password;
pub mod plan;
pub mod record;
pub mod runner;
pub mod store;
#[cfg(windows)]
pub mod windows;

pub use blob::{BlobError, Protector, StoredUser, decode_credentials, encode_credentials};
pub use firewall::{PROXY_PORTS, RulePurpose, RuleSpec, offline_rules};
pub use handshake::{HandshakeError, Message, StepOutcome};
pub use names::{DEFAULT_SLOTS, MAX_SLOTS, NameError, OwnerNames, Role, slot_of};
pub use password::{Password, PasswordError};
pub use plan::{
    Action, CredentialCheck, Credentials, FirewallFacts, ItemState, Observed, PlanError,
    RemoveAction, RemoveStep, StatusLine, StatusReport, Step, StepId, UserFacts, plan_remove,
    plan_setup, status_report,
};
pub use record::{DENIED_LOGON_RIGHTS, Record, RecordError, UndoEntry};
pub use store::{
    LoadError, credentials_file_name, credentials_path, grants_path, load_user, parse_grants,
};

/// What `kgraf sandbox setup` prints on a platform that needs no setup.
pub fn no_setup_needed_notice() -> String {
    format!(
        "The sandbox on {} needs no setup: it uses the platform's own isolation \
         and no administrator rights.",
        std::env::consts::OS
    )
}
