//! `kgraf sandbox setup`, `remove` and `status` on Windows: read the machine,
//! plan with the platform-neutral code, and carry the plan out.
//!
//! The user's own process ([`setup`], [`remove`]) creates the pipe, starts the
//! elevated step through UAC, shows its progress and stores the passwords it
//! sends. The elevated step ([`elevated_main`]) changes the machine and writes
//! the undo record after every step, so a run cut short can still be undone.

use std::path::{Path, PathBuf};

use super::super::acl::{CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS};
use super::super::blob::{StoredUser, decode_credentials, decode_users, encode_credentials};
use super::super::firewall::{offline_rules, rule_users};
use super::super::handshake::{Event, Message, Receiver, StepOutcome, new_nonce, pipe_name};
use super::super::names::{MAX_SLOTS, OwnerNames, Role};
use super::super::password::Password;
use super::super::plan::{
    Action, CredentialCheck, Credentials, Observed, RemoveAction, StatusReport, StepId,
    check_passwords, elevated_params, failure_leaves, plan_remove, plan_setup,
    stale_closed_entries, status_report,
};
use super::super::record::{DENIED_LOGON_RIGHTS, Record, UndoEntry};
use super::super::slots::roots_in_text;
use super::super::store::{grants_path, load_user, parse_grants, slots_dir};
use super::super::wfp::{WfpKeys, filter_users, guid_text};
use super::{accounts, dpapi::Dpapi, elevate, firewall, pipe, secure, wfp};
use crate::error::SandboxError;

fn setup_error(step: &'static str, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: std::io::Error::other(why.to_string()),
    }
}

/// The folders under ProgramData that hold the undo records.
const RECORD_FOLDERS: [&str; 2] = ["kGraf", "sandbox"];

fn record_file_name(names: &OwnerNames) -> String {
    format!("record-{}.txt", names.group())
}

/// The undo record's path for the elevated step, with its folders made or
/// checked so that no one but an administrator can change them
/// (`secure::trusted_folder`). Every user may create a folder under
/// ProgramData, so the step never writes through a folder it did not check.
fn trusted_record_path(
    names: &OwnerNames,
    create: bool,
) -> Result<Option<(PathBuf, secure::TrustedFolder)>, SandboxError> {
    let base = secure::program_data()?;
    let me = accounts::current_user_sid()?;
    let dir = if create {
        Some(secure::trusted_folder(&base, &RECORD_FOLDERS, &me)?)
    } else {
        secure::existing_trusted_folder(&base, &RECORD_FOLDERS, &me)?
    };
    Ok(dir.map(|d| (d.path.join(record_file_name(names)), d)))
}

/// What the machine holds for `names`. Logon rights are read only when
/// `with_rights` (it needs administrator rights).
pub fn observe(names: &OwnerNames, with_rights: bool) -> Result<Observed, SandboxError> {
    let mut o = Observed::default();
    for role in names.roles() {
        let user = names.user(role);
        if let Some(f) = accounts::user_facts(&user)? {
            if with_rights {
                o.rights
                    .insert(user.clone(), Some(accounts::rights_of(&f.sid)?));
            } else {
                o.rights.insert(user.clone(), None);
            }
            o.users.insert(user.clone(), f);
        }
        if accounts::is_hidden(&user)? {
            o.hidden.insert(user);
        }
    }
    o.group = accounts::group_comment(&names.group())?;
    if o.group.is_some() {
        o.group_members = accounts::group_members(&names.group())?;
    }
    o.rules = firewall::existing(&names.rules())?;
    o.firewall = firewall::facts();
    if o.group.is_some() {
        o.group_sid = accounts::sid_of(&names.group())?;
    }
    o.closable = secure::closable_folders()?;
    for folder in secure::folders_to_close()? {
        let closed = match &o.group_sid {
            None => Some(false),
            Some(sid) => secure::closed_now(Path::new(&folder), sid).ok(),
        };
        o.closed.insert(folder, closed);
    }
    match wfp::existing(&WfpKeys::derive(names)) {
        Ok(state) => {
            o.wfp = state.present;
            o.wfp_disabled = state.disabled;
        }
        Err(why) => o.wfp_unreadable = Some(why),
    }
    Ok(o)
}

fn read_record(path: &Path) -> Result<Option<Record>, String> {
    match secure::read_no_follow(path) {
        Ok(Some(text)) => Record::parse(&text).map(Some).map_err(|e| e.to_string()),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Write the record at `path`, whose folder [`trusted_record_path`] checked.
fn write_record(path: &Path, record: &Record) -> Result<(), SandboxError> {
    secure::replace_file(path, record.to_text().as_bytes())
        .map_err(|e| setup_error("write the undo record", e))
}

/// Deny the sandbox group the right to create entries in each folder every
/// local user may write, and list each in the record as soon as it is done,
/// so a run cut short can still be undone.
fn close_folders(
    names: &OwnerNames,
    observed: &Observed,
    record: &mut Record,
    path: &Path,
) -> Result<(), SandboxError> {
    let sid = accounts::sid_of(&names.group())?
        .ok_or_else(|| setup_error("find the sandbox group", "missing"))?;
    // Every folder is tried, and the failures are reported together: one
    // folder an administrator may not write (`%SystemRoot%\Tasks` where
    // Administrators hold less than full control, a drive that cannot be
    // opened) would otherwise stop the setup at the first one, after the
    // accounts, the rights and the rules already exist, and the owner would
    // learn of the next one only on the next run.
    let mut failures: Vec<String> = Vec::new();
    // A generation an earlier setup left behind, whose group was deleted
    // outside kGraf: its entries are taken off and its record lines dropped
    // before this setup writes its own. Without this each setup added another
    // ten entries and ten more lines, and `remove` refused the lot. A stale
    // entry that cannot be taken off joins the failures above instead of
    // stopping the step, so it can never be the one thing that keeps a setup
    // from finishing.
    for entry in stale_closed_entries(names, record, observed, &accounts::sid_resolves) {
        if let UndoEntry::Closed {
            sid: old,
            path: folder,
            mask,
            flags,
        } = &entry
        {
            let at = Path::new(folder);
            if let Err(e) = secure::reopen_folder(at, old, *mask, *flags) {
                let held_by = secure::owner_of(at);
                failures.push(format!(
                    "{folder}, which {held_by} owns: the deny entry of an earlier setup, for \
                     {old}, could not be taken off: {e}"
                ));
                continue;
            }
            record.remove(&entry);
            write_record(path, record)?;
        }
    }
    for folder in secure::folders_to_close()? {
        let at = Path::new(&folder);
        if let Err(e) = secure::close_folder(at, &sid) {
            let held_by = secure::owner_of(at);
            failures.push(format!("{folder}, which {held_by} owns: {e}"));
            continue;
        }
        record.add(UndoEntry::Closed {
            sid: sid.clone(),
            path: folder,
            mask: CLOSED_FOLDER_DENY,
            flags: CLOSED_FOLDER_FLAGS,
        });
        write_record(path, record)?;
    }
    if !failures.is_empty() {
        return Err(setup_error(
            "close the folders every local user may write",
            format!(
                "{} of them could not be closed to the sandbox users. Each is named with the \
                 principal that owns it and the reason; the others were closed and are in \
                 the undo record. {}",
                failures.len(),
                failures.join("; ")
            ),
        ));
    }
    Ok(())
}

// ---- the elevated step ------------------------------------------------------

/// What the elevated step was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElevatedAction {
    Setup,
    Remove,
}

/// The elevated step's arguments, as the user's process passed them.
#[derive(Clone, Debug)]
pub struct ElevatedArgs {
    pub action: ElevatedAction,
    pub pipe: String,
    pub nonce: String,
    pub owner_pid: u32,
    pub owner_sid: String,
    pub tag: Option<String>,
    /// How many worker slots a setup makes users for. Required for a setup:
    /// without it the step would make no slot users while the owner's
    /// process asked for them.
    pub slots: Option<u8>,
}

/// Run the elevated step. Returns the process exit code; every failure is
/// also sent over the pipe so the user's terminal shows it.
pub fn elevated_main(args: &ElevatedArgs) -> i32 {
    let Ok(mut client) = pipe::connect(&args.pipe, args.owner_pid) else {
        return 2;
    };
    let mut send = |m: Message| client.send(&m.encode()).is_ok();
    if !send(Message::Hello {
        nonce: args.nonce.clone(),
    }) {
        return 2;
    }
    let result = OwnerNames::derive(&args.owner_sid, args.tag.as_deref())
        .map_err(|e| ("names".to_owned(), e.to_string()))
        .and_then(|names| match (args.action, args.slots) {
            (ElevatedAction::Setup, None) => Err((
                "names".to_owned(),
                "the setup step was not told how many worker slots to make".to_owned(),
            )),
            (ElevatedAction::Setup, Some(n)) if n > MAX_SLOTS => Err((
                "names".to_owned(),
                format!("--slots must be {MAX_SLOTS} or fewer"),
            )),
            (_, slots) => Ok(names.with_slots(slots.unwrap_or(0))),
        })
        .and_then(|names| match args.action {
            ElevatedAction::Setup => run_setup(&names, &mut send),
            ElevatedAction::Remove => run_remove(&names, &mut send),
        });
    match result {
        Ok(()) => {
            send(Message::End);
            0
        }
        Err((key, message)) => {
            send(Message::Fail { key, message });
            1
        }
    }
}

type Failure = (String, String);

fn run_setup(names: &OwnerNames, send: &mut dyn FnMut(Message) -> bool) -> Result<(), Failure> {
    let fail = |key: &str, e: &dyn std::fmt::Display| (key.to_owned(), e.to_string());
    let observed = observe(names, true).map_err(|e| fail("observe", &e))?;
    let steps = plan_setup(names, &observed).map_err(|e| fail("plan", &e))?;
    // `_hold` keeps the record's folder and every folder above it open
    // until this step ends, so none of them can be renamed away between the
    // check and the writes that follow.
    let (path, _hold) = trusted_record_path(names, true)
        .map_err(|e| fail("record", &e))?
        .ok_or_else(|| {
            fail(
                "record",
                &"the undo record folder is missing after it was made",
            )
        })?;
    let mut record = match read_record(&path) {
        Ok(Some(r)) if r.owner_sid == names.owner_sid() => r,
        _ => Record::new(names.owner_sid(), names.tag()),
    };
    let mut passwords: Vec<(Role, Password)> = Vec::new();
    for role in names.roles() {
        let parts = names.password_forbidden_parts(role);
        let p = Password::generate(&parts).map_err(|e| fail("password", &e))?;
        passwords.push((role, p));
    }
    let password_of = |role: Role| {
        passwords
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, p)| p.expose().to_owned())
            .unwrap_or_default()
    };
    for step in &steps {
        let key = step.id.key();
        let outcome = match step.action {
            Action::Create => StepOutcome::Created,
            Action::Update => StepOutcome::Updated,
            Action::Verify => StepOutcome::Verified,
        };
        let done: Result<(), SandboxError> = match step.id {
            StepId::CheckFirewall => Ok(()),
            StepId::User(role) => {
                let user = names.user(role);
                let comment = names.user_comment(role);
                let pw = password_of(role);
                match step.action {
                    Action::Create => accounts::create_user(&user, &pw, &comment),
                    _ => accounts::update_user(&user, &pw, &comment),
                }
            }
            StepId::NotAdmin(role) => match accounts::user_facts(&names.user(role)) {
                Ok(Some(f)) if f.in_administrators => Err(setup_error(
                    "check a sandbox user",
                    "the user is an administrator",
                )),
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(setup_error("check a sandbox user", "the user is missing")),
                Err(e) => Err(e),
            },
            StepId::Group => {
                let group = names.group();
                let made = if step.action == Action::Create {
                    accounts::create_group(&group, &names.group_comment())
                } else {
                    Ok(())
                };
                made.and_then(|()| {
                    names
                        .roles()
                        .into_iter()
                        .try_for_each(|r| accounts::add_member(&group, &names.user(r)))
                })
            }
            StepId::Rights(role) => accounts::sid_of(&names.user(role))
                .and_then(|s| s.ok_or_else(|| setup_error("find a sandbox user", "missing")))
                .and_then(|sid| accounts::add_rights(&sid, &DENIED_LOGON_RIGHTS)),
            StepId::Hide(role) => accounts::hide(&names.user(role)),
            StepId::Rule(purpose) => sids_of(&rule_users(names)).and_then(|sids| {
                let refs: Vec<&str> = sids.iter().map(String::as_str).collect();
                offline_rules(names, &refs)
                    .into_iter()
                    .find(|r| r.purpose == purpose)
                    .ok_or_else(|| setup_error("plan a firewall rule", "no such rule"))
                    .and_then(|spec| firewall::put(&spec))
            }),
            StepId::CloseFolders => close_folders(names, &observed, &mut record, &path),
            StepId::DnsFilter => sids_of(&filter_users(names)).and_then(|sids| {
                let refs: Vec<&str> = sids.iter().map(String::as_str).collect();
                wfp::put(names, &refs)
            }),
            StepId::Record => Ok(()),
        };
        done.map_err(|e| {
            // Fail loud: name what a failure of this step leaves behind. A
            // re-run that fails here keeps the earlier setup's credentials,
            // so commands keep running as a sandbox user.
            let left = failure_leaves(step.id);
            if left.is_empty() {
                fail(&key, &e)
            } else {
                fail(&key, &format!("{e}; {left}"))
            }
        })?;
        for entry in &step.undo {
            record.add(entry.clone());
        }
        write_record(&path, &record).map_err(|e| fail("record", &e))?;
        if !send(Message::Step { key, outcome }) {
            return Err(fail("pipe", &"the user's process stopped listening"));
        }
    }
    for (role, password) in passwords {
        if !send(Message::Password { role, password }) {
            return Err(fail("pipe", &"the user's process stopped listening"));
        }
    }
    Ok(())
}

fn run_remove(names: &OwnerNames, send: &mut dyn FnMut(Message) -> bool) -> Result<(), Failure> {
    let fail = |key: &str, e: &dyn std::fmt::Display| (key.to_owned(), e.to_string());
    // No folder is no record: nothing to undo. A folder that exists but
    // that others could change is refused before anything is read from it
    // or written to it.
    let Some((path, _hold)) = trusted_record_path(names, false).map_err(|e| fail("record", &e))?
    else {
        return Ok(());
    };
    let mut record = match read_record(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => return Err(fail("record", &e)),
    };
    let observed = observe(names, true).map_err(|e| fail("observe", &e))?;
    let steps = plan_remove(names, &record, &observed, &accounts::sid_resolves)
        .map_err(|e| fail("plan", &e))?;
    for step in steps {
        let key = undo_key(&step.entry);
        // Kept: this removal may not undo the entry (the plan says why). The
        // record keeps it, every other step still runs, and the owner is
        // told, so one entry kGraf cannot judge never blocks a removal.
        if step.action == RemoveAction::Kept {
            let message = step
                .note
                .unwrap_or_else(|| "it is not one this removal may undo".to_owned());
            if !send(Message::Note { key, message }) {
                return Err(fail("pipe", &"the user's process stopped listening"));
            }
            continue;
        }
        let outcome = if step.action == RemoveAction::AlreadyGone {
            StepOutcome::AlreadyGone
        } else {
            let done: Result<(), SandboxError> = match &step.entry {
                UndoEntry::Rule(n) => firewall::remove(n).map(drop),
                UndoEntry::Hidden(n) => accounts::unhide(n),
                UndoEntry::Rights { user, rights } => match accounts::sid_of(user) {
                    Ok(Some(sid)) => {
                        let r: Vec<&str> = rights.iter().map(String::as_str).collect();
                        accounts::remove_rights(&sid, &r)
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                },
                UndoEntry::Group(n) => accounts::delete_group(n).map(drop),
                UndoEntry::User(n) => accounts::delete_user(n).map(drop),
                UndoEntry::Closed {
                    sid,
                    path,
                    mask,
                    flags,
                } => secure::reopen_folder(Path::new(path), sid, *mask, *flags),
                UndoEntry::Wfp { object, key } => wfp::remove(*object, *key).map(drop),
            };
            done.map_err(|e| fail(&key, &e))?;
            StepOutcome::Removed
        };
        record.remove(&step.entry);
        write_record(&path, &record).map_err(|e| fail("record", &e))?;
        if !send(Message::Step { key, outcome }) {
            return Err(fail("pipe", &"the user's process stopped listening"));
        }
    }
    if record.is_empty() {
        secure::delete_no_follow(&path).map_err(|e| fail("record", &e))?;
    }
    Ok(())
}

/// A one-word key for a removal step's pipe message.
fn undo_key(e: &UndoEntry) -> String {
    match e {
        UndoEntry::User(n) => format!("remove-user:{n}"),
        UndoEntry::Group(n) => format!("remove-group:{n}"),
        UndoEntry::Rights { user, .. } => format!("remove-rights:{user}"),
        UndoEntry::Hidden(n) => format!("remove-hidden:{n}"),
        UndoEntry::Rule(n) => format!("remove-rule:{n}"),
        UndoEntry::Closed { path, .. } => format!("remove-closed:{path}"),
        UndoEntry::Wfp { object, key } => format!("remove-{}:{}", object.key(), guid_text(*key)),
    }
}

// ---- the user's own process -------------------------------------------------

/// How to start the elevated step: this program's path and the words before
/// the step's own arguments (e.g. `sandbox elevated`).
pub struct Launch<'a> {
    pub exe: &'a str,
    pub subcommand: &'a str,
}

/// Run the elevated step for `action` and stream its events to `on_event`.
fn drive(
    names: &OwnerNames,
    launch: &Launch<'_>,
    action: ElevatedAction,
    on_event: &mut dyn FnMut(&Event),
) -> Result<Receiver, SandboxError> {
    let nonce = new_nonce().map_err(|e| setup_error("make the setup pipe's nonce", e))?;
    let name = pipe_name(&nonce);
    let server = pipe::create(&name, names.owner_sid())?;
    let pid = std::process::id();
    let params = format!(
        "{} {}",
        launch.subcommand,
        elevated_params(names, action == ElevatedAction::Setup, &name, &nonce, pid)
    );
    let child = elevate::run_elevated(launch.exe, &params)?;
    let lines = server.accept(&child.process, child.pid)?;
    let mut receiver = Receiver::new();
    for line in lines {
        let line = line.map_err(|e| setup_error("read the setup pipe", e))?;
        let event = receiver
            .take(&line, &nonce)
            .map_err(|e| setup_error("read the setup pipe", e))?;
        on_event(&event);
        if let Event::Failed { key, message } = &event {
            return Err(setup_error(
                "the elevated setup step",
                format!("step {key} failed: {message}"),
            ));
        }
        if receiver.finished() {
            break;
        }
    }
    Ok(receiver)
}

/// The SIDs of `users`, each of which setup has made by now. The firewall
/// rules name [`rule_users`] and the DNS filter [`filter_users`]: the offline
/// user and each worker slot's user without the network. One rule names them
/// all, so the rule names stay the same whatever the slot count is.
fn sids_of(users: &[String]) -> Result<Vec<String>, SandboxError> {
    users
        .iter()
        .map(|user| {
            accounts::sid_of(user)?
                .ok_or_else(|| setup_error("find a sandbox user", format!("{user} is missing")))
        })
        .collect()
}

/// `kgraf sandbox setup`: create or refresh the sandbox users and rules, then
/// store their passwords for this user at `store`.
pub fn setup(
    names: &OwnerNames,
    launch: &Launch<'_>,
    store: &Path,
    on_event: &mut dyn FnMut(&Event),
) -> Result<(), SandboxError> {
    let receiver = drive(names, launch, ElevatedAction::Setup, on_event)?;
    let passwords = receiver
        .into_passwords()
        .map_err(|e| setup_error("receive the sandbox passwords", e))?;
    let received: Vec<Role> = passwords.iter().map(|(role, _)| *role).collect();
    check_passwords(names, &received)
        .map_err(|e| setup_error("receive the sandbox passwords", e))?;
    let mut entries = Vec::new();
    for (role, password) in &passwords {
        let user = names.user(*role);
        let sid = accounts::sid_of(&user)?
            .ok_or_else(|| setup_error("find a sandbox user", "missing after setup"))?;
        entries.push((
            StoredUser {
                role: *role,
                name: user,
                sid,
            },
            password,
        ));
    }
    let refs: Vec<(StoredUser, &Password)> = entries.iter().map(|(u, p)| (u.clone(), *p)).collect();
    let bytes =
        encode_credentials(&refs, &Dpapi).map_err(|e| setup_error("seal the passwords", e))?;
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir).map_err(|e| setup_error("create the password folder", e))?;
    }
    let tmp = store.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| setup_error("store the passwords", e))?;
    std::fs::rename(&tmp, store).map_err(|e| setup_error("store the passwords", e))
}

/// `kgraf sandbox remove`: undo what the record lists, then delete the stored
/// passwords at `store`.
///
/// Returns what is left behind, one line each: a folder whose entries for
/// the sandbox users could not be removed, or a list that could not be read.
/// The users, the firewall rules and the DNS filter are removed whatever is
/// left, so a folder kGraf cannot reach now (an unready drive, a junction, a
/// list this user cannot write) never keeps a machine set up for good. An
/// entry for a deleted user's SID grants nothing to anyone.
pub fn remove(
    names: &OwnerNames,
    launch: &Launch<'_>,
    store: &Path,
    on_event: &mut dyn FnMut(&Event),
) -> Result<Vec<String>, SandboxError> {
    let mut left = revoke_grants(store);
    left.extend(revoke_slot_grants(store));
    drive(names, launch, ElevatedAction::Remove, on_event)?;
    match std::fs::remove_file(store) {
        Ok(()) => Ok(left),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(left),
        Err(e) => Err(setup_error("delete the stored passwords", e)),
    }
}

/// `kgraf sandbox status`: what exists and what is missing. Needs no
/// administrator rights (logon rights are reported as not read).
pub fn status(names: &OwnerNames, store: &Path) -> Result<StatusReport, SandboxError> {
    // Read every possible slot user, then report the slots this machine
    // actually has: a report built from a fixed count would call the slots
    // of a smaller setup missing, and miss those of a larger one.
    let probe = names.clone().with_slots(MAX_SLOTS);
    let observed = observe(&probe, false)?;
    // Both users of a slot are counted, so a slot with one user missing is
    // reported as missing that user.
    let count = |role: fn(u8) -> Role| {
        (0..MAX_SLOTS)
            .take_while(|i| observed.users.contains_key(&probe.user(role(*i))))
            .count()
    };
    let found = count(Role::Slot).max(count(Role::OnlineSlot));
    let names = &names.clone().with_slots(u8::try_from(found).unwrap_or(0));
    // The record is read through the same folder judgment the elevated step
    // uses, not by name: a junction in place of the record's folder would
    // otherwise feed the owner a record of someone else's choosing, a
    // removal that looks complete among them. A folder that does not pass
    // is reported as a record that cannot be trusted, with the reason.
    let record = trusted_record_path(names, false)
        .map_err(|e| format!("the undo record's folder is not trusted: {e}"))
        .and_then(|found| match found {
            None => Ok(None),
            Some((path, _hold)) => read_record(&path),
        });
    let credentials = match std::fs::read(store) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Credentials::Missing,
        Err(e) => Credentials::Unreadable(e.to_string()),
        Ok(bytes) => match decode_credentials(&bytes, &Dpapi) {
            Ok(users) => Credentials::Stored(
                users
                    .into_iter()
                    .map(|(u, p)| {
                        let check = match accounts::logon_works(&u.name, p.expose()) {
                            Ok(()) => CredentialCheck::LogonWorks,
                            Err(why) => CredentialCheck::LogonFailed(why),
                        };
                        (u.name, check)
                    })
                    .collect(),
            ),
            Err(e) => match decode_users(&bytes) {
                Ok(_) => Credentials::Unreadable(e.to_string()),
                Err(e) => Credentials::Unreadable(e.to_string()),
            },
        },
    };
    Ok(status_report(
        names,
        &observed,
        record.as_ref().map(Option::as_ref).map_err(Clone::clone),
        &credentials,
    ))
}

/// The sandbox user of `role` and its password from the store at `store`,
/// for a command that starts as that user. `None` when the store does not
/// exist (setup has not run). A store that exists but cannot be read or
/// opened is an error, never `None`: the caller would otherwise fall back to
/// a weaker sandbox without saying why.
pub fn stored_user(
    store: &Path,
    role: Role,
) -> Result<Option<(StoredUser, Password)>, SandboxError> {
    let bytes = match std::fs::read(store) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(setup_error("read the stored sandbox passwords", e)),
    };
    load_user(&bytes, role, &Dpapi)
        .map(Some)
        .map_err(|e| setup_error("open the stored sandbox password", e))
}

/// The SIDs of the stored sandbox users, or why they could not be read.
/// Without them nothing can be revoked, and a removal says so instead of
/// stopping: the users are deleted anyway, and an entry for a SID that no
/// account has grants nothing to anyone.
fn stored_sids(store: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(store).map_err(|e| format!("{}: {e}", store.display()))?;
    let users = decode_users(&bytes).map_err(|e| format!("{}: {e}", store.display()))?;
    Ok(users.into_iter().map(|(u, _)| u.sid).collect())
}

/// Remove each slot user's write grant from every folder the slot table
/// beside `store` names ([`super::super::slots`]), then delete the table and
/// its lock files. Runs before the users are removed. A folder that is gone
/// is skipped.
///
/// Every folder is tried, and what could not be revoked is returned for the
/// owner to read. The table and its lock files are deleted either way: the
/// users that follow them are deleted too, so a leftover entry names a SID
/// no account has. Before this, the first failure (an unready drive, a
/// junction, a folder whose list this user cannot write, one damaged line)
/// stopped the whole removal, so the users, the firewall rules and the DNS
/// filter stayed on the machine, on this run and on every run after it.
fn revoke_slot_grants(store: &Path) -> Vec<String> {
    let mut left = Vec::new();
    let dir = slots_dir(store);
    let path = dir.join("table.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            left.push(format!(
                "the sandbox slot table {} could not be read ({e}), so the write grants it \
                 lists are still in place",
                path.display()
            ));
            String::new()
        }
    };
    let (roots, damaged) = roots_in_text(&text);
    if !damaged.is_empty() {
        left.push(format!(
            "the sandbox slot table {} has {} damaged line(s) ({}), whose folders could not \
             be read; remove any entry for a kGraf sandbox user there with `icacls`",
            path.display(),
            damaged.len(),
            damaged
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    left.extend(revoke_each(store, roots));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => left.push(format!(
            "the sandbox slot folder {} could not be deleted: {e}",
            dir.display()
        )),
    }
    left
}

/// Remove the sandbox users' entries from every folder of the owner's that
/// the grants list beside `store` names, then delete the list. Runs before
/// the users are removed, as the owner, who may rewrite the DACLs of the
/// owner's own folders. Every folder is tried; what is left is returned, as
/// in [`revoke_slot_grants`].
fn revoke_grants(store: &Path) -> Vec<String> {
    let mut left = Vec::new();
    let list = grants_path(store);
    let text = match std::fs::read_to_string(&list) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return left,
        Err(e) => {
            return vec![format!(
                "the sandbox grants list {} could not be read ({e}), so the entries it lists \
                 are still in place",
                list.display()
            )];
        }
    };
    left.extend(revoke_each(store, parse_grants(&text)));
    match std::fs::remove_file(&list) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => left.push(format!(
            "the sandbox grants list {} could not be deleted: {e}",
            list.display()
        )),
    }
    left
}

/// Revoke the stored sandbox users' entries on every folder of `roots`,
/// and describe each folder that kept them.
///
/// The notes the removal pass leaves on a folder (a child it could not open
/// or write) are dropped here: the users themselves go right after this, so
/// an entry a child kept names a user that no longer exists, which `icacls`
/// shows as a raw SID the owner can take off. A folder that kept its entries
/// as a whole is a leftover, and is reported.
fn revoke_each(store: &Path, roots: Vec<PathBuf>) -> Vec<String> {
    if roots.is_empty() {
        return Vec::new();
    }
    let sids = match stored_sids(store) {
        Ok(sids) => sids,
        Err(why) => {
            return vec![format!(
                "the stored sandbox users could not be read ({why}), so the entries on {} \
                 folder(s) are still in place; remove them with `icacls` if the folders stay",
                roots.len()
            )];
        }
    };
    let sids: Vec<&str> = sids.iter().map(String::as_str).collect();
    roots
        .into_iter()
        .filter_map(|path| {
            crate::sandbox::revoke_sandbox_grant(&path, &sids)
                .err()
                .map(|e| {
                    format!(
                        "{} keeps its entries for the sandbox users: {e}",
                        path.display()
                    )
                })
        })
        .collect()
}
