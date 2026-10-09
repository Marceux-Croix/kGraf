//! Whether this process runs sandboxed commands as the sandbox user, read
//! once per process (decision 0097).
//!
//! - No stored passwords: `kgraf sandbox setup` has not run. Commands use
//!   the token sandbox of decision 0011, and every notice that names its
//!   gaps says how to close them. With [`TAG_ENV`] set, this is an error
//!   instead: the tag asked for one setup's users by name.
//! - Stored passwords that open, and the runner beside this executable:
//!   commands run as the sandbox users.
//! - Anything between (a store that does not open, a store for other names,
//!   one user missing, no runner): an error for every sandboxed command.
//!   Setup ran, so a command must not quietly get less confinement than it
//!   promised (fail loud).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use super::super::names::{OwnerNames, Role};
use super::super::password::Password;
use super::super::runner::RUNNER_NAME;
use super::super::slots::{Lease, Leases, Pool};
use super::super::store::{
    credentials_path, grants_path, load_slot_users, missing_store, slots_dir,
};
use super::{accounts, dpapi::Dpapi, setup::stored_user};
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
    /// One user without the network per worker slot, in slot order, as the
    /// setup created them. Empty where the setup made none, and then
    /// [`Self::offline`] is the one user of its pool.
    pub slots: Vec<LoadedUser>,
    /// One user with the network open per worker slot, as many as
    /// [`Self::slots`]. Empty where the setup made none, and then
    /// [`Self::online`] is the one user of its pool.
    pub online_slots: Vec<LoadedUser>,
    /// Which folder holds which slot ([`super::super::slots`]).
    pub leases: Leases,
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

    /// The user of `role`. A slot the setup did not make is an error, never
    /// another user: two folders must not run as one user without a word.
    pub(crate) fn user(&self, role: Role) -> Result<&LoadedUser, SandboxError> {
        let found = match role {
            Role::Offline => Some(&self.offline),
            Role::Online => Some(&self.online),
            Role::Slot(i) => self.slots.get(usize::from(i)),
            Role::OnlineSlot(i) => self.online_slots.get(usize::from(i)),
        };
        found.ok_or_else(|| error(format!("the setup made no sandbox user for {role}")))
    }

    /// The slot of `pool` a command whose writable roots are `roots` runs
    /// in: the worker's own (`worker`, which must be held for `roots[0]`),
    /// else the one this process holds for that folder, else one taken for
    /// the command alone. Held as long as the result lives. A slot that
    /// moves to this folder first loses its write grants on the folders it
    /// held. While every slot of the pool is held for another folder, it
    /// waits up to `wait`, then is refused.
    ///
    /// The notes beside the lease are what those grant removals left behind:
    /// a file whose access list they could not write keeps the entry it had,
    /// and the caller carries that to the owner with the command's output.
    pub(crate) fn lease(
        &self,
        pool: Pool,
        worker: Option<&crate::sandbox::WorkerSlot>,
        roots: &[PathBuf],
        wait: std::time::Duration,
    ) -> Result<(Arc<Lease>, Vec<String>), SandboxError> {
        let slot_error = |e: super::super::slots::SlotError| error(e);
        if let Some(worker) = worker.filter(|w| w.0.pool() == pool) {
            let first = roots
                .first()
                .ok_or_else(|| slot_error(super::super::slots::SlotError::NoRoot))?;
            if !worker.0.is_for(first) {
                return Err(slot_error(super::super::slots::SlotError::OtherFolder {
                    held: worker.0.primary().to_path_buf(),
                    wants: first.clone(),
                }));
            }
            worker.0.note_roots(roots).map_err(slot_error)?;
            return Ok((worker.0.clone(), Vec::new()));
        }
        let mut notes: Vec<String> = Vec::new();
        let lease = {
            let mut revoke = |role: Role, path: &Path| -> Result<(), String> {
                let user = self.user(role).map_err(|e| e.to_string())?;
                let left = crate::sandbox::revoke_sandbox_grant(path, &[user.sid.as_str()])
                    .map_err(|e| e.to_string())?;
                notes.extend(left);
                Ok(())
            };
            self.leases
                .lease_waiting(pool, roots, &mut revoke, wait)
                .map(|(lease, _)| lease)
                .map_err(slot_error)?
        };
        Ok((lease, notes))
    }

    /// Every user, for a grant that any of them may need.
    pub(crate) fn all(&self) -> Vec<&LoadedUser> {
        std::iter::once(&self.offline)
            .chain(std::iter::once(&self.online))
            .chain(self.slots.iter())
            .chain(self.online_slots.iter())
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
    // No configuration folder, or no stored user: setup has not run, and the
    // token sandbox runs, unless a tag asked for one setup by name.
    let Some(store) = credentials_path(tag.as_deref()) else {
        missing_store(tag.as_deref(), None).map_err(error)?;
        return Ok(None);
    };
    let Some((offline, offline_pw)) = stored_user(&store, Role::Offline)? else {
        missing_store(tag.as_deref(), Some(&store)).map_err(error)?;
        return Ok(None);
    };
    let (online, online_pw) = stored_user(&store, Role::Online)?
        .ok_or_else(|| error("the stored passwords file holds no online sandbox user"))?;
    // The slot users the setup created, from slot 0 up to the first gap: a
    // store that names slot 0 and slot 2 but not slot 1 is not one this
    // kGraf wrote, and acting on part of it would run two worktrees as one
    // user without saying so.
    let slots = stored_slots(&store, Role::Slot)?;
    let online_slots = stored_slots(&store, Role::OnlineSlot)?;
    if slots.len() != online_slots.len() {
        return Err(error(format!(
            "the stored passwords file names {} worker slots without the network and {} with \
             it; setup makes as many of each. Run `kgraf sandbox setup` again",
            slots.len(),
            online_slots.len()
        )));
    }
    let mut checked: Vec<(&super::super::blob::StoredUser, Role)> =
        vec![(&offline, Role::Offline), (&online, Role::Online)];
    for (i, (user, _)) in slots.iter().enumerate() {
        checked.push((user, Role::Slot(u8::try_from(i).unwrap_or(u8::MAX))));
    }
    for (i, (user, _)) in online_slots.iter().enumerate() {
        checked.push((user, Role::OnlineSlot(u8::try_from(i).unwrap_or(u8::MAX))));
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
    let count = u8::try_from(slots.len()).unwrap_or(u8::MAX);
    let loaded = |list: Vec<(super::super::blob::StoredUser, Password)>| -> Vec<LoadedUser> {
        list.into_iter()
            .map(|(user, password)| LoadedUser {
                name: user.name,
                sid: user.sid,
                password,
            })
            .collect()
    };
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
        slots: loaded(slots),
        online_slots: loaded(online_slots),
        leases: Leases::new(slots_dir(&store), count),
        runner,
        grants: grants_path(&store),
    }))
}

/// The stored slot users of `role(0)`, `role(1)`, and on
/// ([`load_slot_users`]). Read only after the offline user was found, so the
/// store exists.
fn stored_slots(
    store: &Path,
    role: fn(u8) -> Role,
) -> Result<Vec<(super::super::blob::StoredUser, Password)>, SandboxError> {
    let bytes = std::fs::read(store)
        .map_err(|e| error(format!("read the stored sandbox passwords: {e}")))?;
    load_slot_users(&bytes, role, &Dpapi)
        .map_err(|e| error(format!("open the stored sandbox passwords: {e}")))
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
