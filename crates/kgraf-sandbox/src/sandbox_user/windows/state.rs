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

use super::super::names::{MAX_SLOTS, OwnerNames, Role, slot_of};
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

/// The sandbox users and the runner that starts commands as them.
#[derive(Debug)]
pub(crate) struct Users {
    pub offline: LoadedUser,
    pub online: LoadedUser,
    /// One user per worker slot, in slot order, as the setup created them.
    /// Empty where the setup made none, and then [`Self::offline`] runs every
    /// command that has no network.
    pub slots: Vec<LoadedUser>,
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
            // A slot the setup did not create falls back to the offline
            // user, which is confined the same way.
            Role::Slot(i) => self.slots.get(usize::from(i)).unwrap_or(&self.offline),
        }
    }

    /// Which user runs a command without the network whose first writable
    /// root is `root`. With slot users present, the slot is the root's name
    /// folded to the slot count, so two commands in different worktrees
    /// usually run as different users and neither can write the other's
    /// files; two commands in one worktree share a user, which they must,
    /// because they share the folder. Without slot users it is always the
    /// offline user.
    pub(crate) fn offline_role_for(&self, root: Option<&Path>) -> Role {
        let count = u8::try_from(self.slots.len()).unwrap_or(u8::MAX);
        match (count, root) {
            (0, _) | (_, None) => Role::Offline,
            (count, Some(root)) => Role::Slot(slot_of(root, count)),
        }
    }

    /// Every user, for a grant that either of them may need.
    pub(crate) fn all(&self) -> Vec<&LoadedUser> {
        std::iter::once(&self.offline)
            .chain(std::iter::once(&self.online))
            .chain(self.slots.iter())
            .collect()
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
    // The slot users the setup created, from slot 0 up to the first gap: a
    // store that names slot 0 and slot 2 but not slot 1 is not one this
    // kGraf wrote, and acting on part of it would run two worktrees as one
    // user without saying so.
    let mut slots: Vec<(super::super::blob::StoredUser, Password)> = Vec::new();
    for i in 0..MAX_SLOTS {
        match stored_user(&store, Role::Slot(i))? {
            Some(found) => slots.push(found),
            None => break,
        }
    }
    for i in u8::try_from(slots.len()).unwrap_or(u8::MAX)..MAX_SLOTS {
        if stored_user(&store, Role::Slot(i))?.is_some() {
            return Err(error(format!(
                "the stored passwords file names worker slot {i} but not every slot below it; \
                 run `kgraf sandbox setup` again"
            )));
        }
    }
    let mut checked: Vec<(&super::super::blob::StoredUser, Role)> =
        vec![(&offline, Role::Offline), (&online, Role::Online)];
    for (i, (user, _)) in slots.iter().enumerate() {
        checked.push((user, Role::Slot(u8::try_from(i).unwrap_or(u8::MAX))));
    }
    for (user, role) in checked {
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
        slots: slots
            .into_iter()
            .map(|(user, password)| LoadedUser {
                name: user.name,
                sid: user.sid,
                password,
            })
            .collect(),
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
