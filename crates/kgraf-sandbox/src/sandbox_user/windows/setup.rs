//! `kgraf sandbox setup`, `remove` and `status` on Windows: read the machine,
//! plan with the platform-neutral code, and carry the plan out.
//!
//! The user's own process ([`setup`], [`remove`]) creates the pipe, starts the
//! elevated step through UAC, shows its progress and stores the passwords it
//! sends. The elevated step ([`elevated_main`]) changes the machine and writes
//! the undo record after every step, so a run cut short can still be undone.

use std::path::{Path, PathBuf};

use super::super::blob::{StoredUser, decode_credentials, decode_users, encode_credentials};
use super::super::firewall::offline_rules;
use super::super::handshake::{Event, Message, Receiver, StepOutcome, new_nonce, pipe_name};
use super::super::names::{OwnerNames, Role};
use super::super::password::Password;
use super::super::plan::{
    Action, CredentialCheck, Credentials, Observed, RemoveAction, StatusReport, StepId,
    plan_remove, plan_setup, status_report,
};
use super::super::record::{DENIED_LOGON_RIGHTS, Record, UndoEntry};
use super::{accounts, dpapi::Dpapi, elevate, firewall, pipe};
use crate::error::SandboxError;

fn setup_error(step: &'static str, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: std::io::Error::other(why.to_string()),
    }
}

/// Where the undo record lives: under ProgramData, written by the elevated
/// step. One file per owner (the group name is derived from the owner).
pub fn record_path(names: &OwnerNames) -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    base.join("kGraf")
        .join("sandbox")
        .join(format!("record-{}.txt", names.group()))
}

/// What the machine holds for `names`. Logon rights are read only when
/// `with_rights` (it needs administrator rights).
pub fn observe(names: &OwnerNames, with_rights: bool) -> Result<Observed, SandboxError> {
    let mut o = Observed::default();
    for role in Role::ALL {
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
    Ok(o)
}

fn read_record(path: &Path) -> Result<Option<Record>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Record::parse(&text).map(Some).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn write_record(path: &Path, record: &Record) -> Result<(), SandboxError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| setup_error("create the undo record folder", e))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, record.to_text()).map_err(|e| setup_error("write the undo record", e))?;
    std::fs::rename(&tmp, path).map_err(|e| setup_error("write the undo record", e))
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
    let path = record_path(names);
    let mut record = match read_record(&path) {
        Ok(Some(r)) if r.owner_sid == names.owner_sid() => r,
        _ => Record::new(names.owner_sid(), names.tag()),
    };
    let parts = names.password_forbidden_parts();
    let mut passwords: Vec<(Role, Password)> = Vec::new();
    for role in Role::ALL {
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
                    Role::ALL
                        .into_iter()
                        .try_for_each(|r| accounts::add_member(&group, &names.user(r)))
                })
            }
            StepId::Rights(role) => accounts::sid_of(&names.user(role))
                .and_then(|s| s.ok_or_else(|| setup_error("find a sandbox user", "missing")))
                .and_then(|sid| accounts::add_rights(&sid, &DENIED_LOGON_RIGHTS)),
            StepId::Hide(role) => accounts::hide(&names.user(role)),
            StepId::Rule(purpose) => accounts::sid_of(&names.user(Role::Offline))
                .and_then(|s| s.ok_or_else(|| setup_error("find the offline user", "missing")))
                .and_then(|sid| {
                    offline_rules(names, &sid)
                        .into_iter()
                        .find(|r| r.purpose == purpose)
                        .ok_or_else(|| setup_error("plan a firewall rule", "no such rule"))
                        .and_then(|spec| firewall::put(&spec))
                }),
            StepId::Record => Ok(()),
        };
        done.map_err(|e| fail(&key, &e))?;
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
    let path = record_path(names);
    let mut record = match read_record(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => return Err(fail("record", &e)),
    };
    let observed = observe(names, true).map_err(|e| fail("observe", &e))?;
    let steps = plan_remove(names, &record, &observed).map_err(|e| fail("plan", &e))?;
    for step in steps {
        let key = undo_key(&step.entry);
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
        let _ = std::fs::remove_file(&path);
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
    let mut params = format!(
        "{} --action {} --pipe {name} --nonce {nonce} --owner-pid {pid} --owner-sid {}",
        launch.subcommand,
        match action {
            ElevatedAction::Setup => "setup",
            ElevatedAction::Remove => "remove",
        },
        names.owner_sid()
    );
    if let Some(tag) = names.tag() {
        params.push_str(&format!(" --tag {tag}"));
    }
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
pub fn remove(
    names: &OwnerNames,
    launch: &Launch<'_>,
    store: &Path,
    on_event: &mut dyn FnMut(&Event),
) -> Result<(), SandboxError> {
    drive(names, launch, ElevatedAction::Remove, on_event)?;
    match std::fs::remove_file(store) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(setup_error("delete the stored passwords", e)),
    }
}

/// `kgraf sandbox status`: what exists and what is missing. Needs no
/// administrator rights (logon rights are reported as not read).
pub fn status(names: &OwnerNames, store: &Path) -> Result<StatusReport, SandboxError> {
    let observed = observe(names, false)?;
    let record = read_record(&record_path(names));
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
