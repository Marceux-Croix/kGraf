//! Whether this process runs sandboxed commands as the sandbox user, read
//! once per process (decision 0097).
//!
//! - No stored passwords: `kgraf sandbox setup` has not run. Commands use
//!   the token sandbox of decision 0011, and every notice that names its
//!   gaps says how to close them.
//! - Stored passwords that open, and the runner beside this executable:
//!   commands run as the sandbox users.
//! - Anything between (a store that does not open, a store for other names,
//!   one user missing, no runner): an error for every sandboxed command.
//!   Setup ran, so a command must not quietly get less confinement than it
//!   promised (fail loud).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::super::names::{OwnerNames, Role};
use super::super::password::Password;
use super::super::runner::RUNNER_NAME;
use super::super::store::{credentials_path, grants_path};
use super::{accounts, setup::stored_user};
use crate::error::SandboxError;

/// Names the sandbox users of a test run (`kgraf sandbox setup --tag`), so
/// a test's commands run as the users that test set up. Unset for the
/// owner's own users.
pub const TAG_ENV: &str = "KGRAF_SANDBOX_TAG";

/// One sandbox user and its password, opened from the store.
pub(crate) struct LoadedUser {
    pub name: String,
    pub sid: String,
    pub password: Password,
}

impl std::fmt::Debug for LoadedUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedUser")
            .field("name", &self.name)
            .field("sid", &self.sid)
            .finish_non_exhaustive()
    }
}

/// Both sandbox users and the runner that starts commands as them.
#[derive(Debug)]
pub(crate) struct Users {
    pub offline: LoadedUser,
    pub online: LoadedUser,
    pub runner: PathBuf,
    /// The list of the owner's folders granted to the sandbox users
    /// ([`grants_path`]).
    pub grants: PathBuf,
}

impl Users {
    /// Add `path` to the grants list. One line per call, appended, so
    /// processes that write at the same time do not lose each other's lines.
    pub(crate) fn record_grant(&self, path: &Path) -> Result<(), SandboxError> {
        use std::io::Write;
        let line = format!("{}\n", path.display());
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.grants)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(|e| error(format!("record a grant in {}: {e}", self.grants.display())))
    }

    pub(crate) fn user(&self, role: Role) -> &LoadedUser {
        match role {
            Role::Offline => &self.offline,
            Role::Online => &self.online,
        }
    }
}

fn error(why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step: "use the Windows sandbox user",
        source: std::io::Error::other(why.to_string()),
    }
}

/// The sandbox users, or `None` when setup has not run. Read once; the
/// result, an error included, holds for the life of the process.
pub(crate) fn users() -> Result<Option<&'static Users>, SandboxError> {
    static USERS: OnceLock<Result<Option<Users>, String>> = OnceLock::new();
    match USERS.get_or_init(|| load().map_err(|e| e.to_string())) {
        Ok(users) => Ok(users.as_ref()),
        Err(why) => Err(error(why)),
    }
}

fn load() -> Result<Option<Users>, SandboxError> {
    let tag = std::env::var(TAG_ENV).ok().filter(|t| !t.is_empty());
    let names = OwnerNames::derive(&accounts::current_user_sid()?, tag.as_deref())
        .map_err(|e| error(format!("{TAG_ENV}: {e}")))?;
    // No configuration folder: setup could not have stored anything.
    let Some(store) = credentials_path(tag.as_deref()) else {
        return Ok(None);
    };
    let Some((offline, offline_pw)) = stored_user(&store, Role::Offline)? else {
        return Ok(None);
    };
    let (online, online_pw) = stored_user(&store, Role::Online)?
        .ok_or_else(|| error("the stored passwords file holds no online sandbox user"))?;
    for (user, role) in [(&offline, Role::Offline), (&online, Role::Online)] {
        let want = names.user(role);
        if user.name != want {
            return Err(error(format!(
                "the stored {role} sandbox user is {}, not {want}, this Windows user's; run \
                 `kgraf sandbox setup` again",
                user.name
            )));
        }
        let sid = accounts::sid_of(&user.name)?.ok_or_else(|| {
            error(format!(
                "the sandbox user {} no longer exists; run `kgraf sandbox setup` again",
                user.name
            ))
        })?;
        if sid != user.sid {
            return Err(error(format!(
                "the sandbox user {} was created again since setup stored it; run `kgraf \
                 sandbox setup` again",
                user.name
            )));
        }
    }
    let exe = std::env::current_exe().map_err(error)?;
    let runner = find_runner(&exe).ok_or_else(|| {
        error(format!(
            "{RUNNER_NAME}.exe is not installed beside {}",
            exe.display()
        ))
    })?;
    Ok(Some(Users {
        offline: LoadedUser {
            name: offline.name,
            sid: offline.sid,
            password: offline_pw,
        },
        online: LoadedUser {
            name: online.name,
            sid: online.sid,
            password: online_pw,
        },
        runner,
        grants: grants_path(&store),
    }))
}

/// The runner beside `exe`; for a test binary in Cargo's `deps` folder, the
/// runner Cargo built one folder up.
pub(crate) fn find_runner(exe: &Path) -> Option<PathBuf> {
    let file = format!("{RUNNER_NAME}.exe");
    let dir = exe.parent()?;
    let beside = dir.join(&file);
    if beside.is_file() {
        return Some(beside);
    }
    if dir.file_name().is_some_and(|n| n == "deps") {
        let above = dir.parent()?.join(&file);
        if above.is_file() {
            return Some(above);
        }
    }
    None
}
