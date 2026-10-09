//! The setup plan, the removal plan and the status report, from what the
//! machine holds now ([`Observed`]).
//!
//! The Windows code reads the machine into [`Observed`] and runs the steps
//! these functions return; it decides nothing itself. So the order, the
//! refusals and the undo list are tested here, on every host.
//!
//! Setup is idempotent: a user, group or rule that exists and carries kGraf's
//! marker for this owner is updated (a user's password is reset), and one
//! that is missing is created. A user or group with kGraf's name but another
//! marker is refused, never taken over. Every step lists what `remove` must
//! undo after it succeeds.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::firewall::RulePurpose;
use super::names::{OwnerNames, Role};
use super::record::{DENIED_LOGON_RIGHTS, Record, UndoEntry};
use super::wfp::{WfpKeys, WfpObject, guid_text};

/// One existing local user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserFacts {
    pub sid: String,
    pub comment: String,
    /// Whether the user is in the local Administrators group.
    pub in_administrators: bool,
}

/// The firewall's state. `None` is "could not be read".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FirewallFacts {
    /// Windows Firewall is on for the domain, private and public profiles.
    pub enabled_all_profiles: Option<bool>,
    /// Rules added to the local policy take effect on every current profile
    /// (`INetFwPolicy2::LocalPolicyModifyState`): no group policy overrides
    /// them.
    pub local_policy_effective: Option<bool>,
}

/// What the machine holds, for the names of one owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    /// Only the users that exist, by name.
    pub users: BTreeMap<String, UserFacts>,
    /// The group's comment, if the group exists.
    pub group: Option<String>,
    pub group_members: BTreeSet<String>,
    /// The names of the firewall rules that exist.
    pub rules: BTreeSet<String>,
    /// Users with a value under Winlogon's `SpecialAccounts\UserList`.
    pub hidden: BTreeSet<String>,
    /// The denied logon rights each user holds; `None` when they could not
    /// be read (reading them needs administrator rights).
    pub rights: BTreeMap<String, Option<BTreeSet<String>>>,
    pub firewall: FirewallFacts,
    /// The SID of the sandbox group, if the group exists.
    pub group_sid: Option<String>,
    /// The folders setup closes to the sandbox group
    /// ([`super::acl::closed_folders`]), each with whether the group's deny
    /// entry is on it; `None` when that could not be read.
    pub closed: BTreeMap<String, Option<bool>>,
    /// The keys of kGraf's WFP objects (provider, sublayer, DNS filter) that
    /// exist.
    pub wfp: BTreeSet<u128>,
    /// Why the WFP objects could not be read, when they could not (reading
    /// them can need administrator rights).
    pub wfp_unreadable: Option<String>,
}

/// One setup step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepId {
    /// The firewall is on and local rules take effect.
    CheckFirewall,
    /// Create the user, or reset the password of the existing one.
    User(Role),
    /// The user is not in the Administrators group.
    NotAdmin(Role),
    /// The group exists and holds both users.
    Group,
    /// Deny the user every logon right but interactive.
    Rights(Role),
    /// Hide the user from the sign-in screen.
    Hide(Role),
    /// Add or update one firewall rule.
    Rule(RulePurpose),
    /// Deny the sandbox group the right to create or change entries in the
    /// folders every local user may write ([`super::acl::closed_folders`]).
    CloseFolders,
    /// Add or refresh the WFP filter that keeps the blocked users from the
    /// DNS Client service ([`super::wfp`]).
    DnsFilter,
    /// Write the undo record.
    Record,
}

impl StepId {
    /// A single word for the pipe messages.
    pub fn key(self) -> String {
        match self {
            StepId::CheckFirewall => "check-firewall".into(),
            StepId::User(r) => format!("user-{}", r.key()),
            StepId::NotAdmin(r) => format!("not-admin-{}", r.key()),
            StepId::Group => "group".into(),
            StepId::Rights(r) => format!("rights-{}", r.key()),
            StepId::Hide(r) => format!("hide-{}", r.key()),
            StepId::Rule(p) => format!("rule-{}", p.key()),
            StepId::CloseFolders => "close-folders".into(),
            StepId::DnsFilter => "dns-filter".into(),
            StepId::Record => "record".into(),
        }
    }

    pub fn from_key(key: &str) -> Option<StepId> {
        let role = |s: &str| Role::from_key(s);
        Some(match key {
            "check-firewall" => StepId::CheckFirewall,
            "group" => StepId::Group,
            "record" => StepId::Record,
            "close-folders" => StepId::CloseFolders,
            "dns-filter" => StepId::DnsFilter,
            _ => {
                if let Some(r) = key.strip_prefix("user-") {
                    StepId::User(role(r)?)
                } else if let Some(r) = key.strip_prefix("not-admin-") {
                    StepId::NotAdmin(role(r)?)
                } else if let Some(r) = key.strip_prefix("rights-") {
                    StepId::Rights(role(r)?)
                } else if let Some(r) = key.strip_prefix("hide-") {
                    StepId::Hide(role(r)?)
                } else {
                    StepId::Rule(RulePurpose::from_key(key.strip_prefix("rule-")?)?)
                }
            }
        })
    }

    /// What the step is about, in words, for the owner's terminal.
    pub fn describe(self, names: &OwnerNames) -> String {
        match self {
            StepId::CheckFirewall => "Windows Firewall is on and local rules take effect".into(),
            StepId::User(r) => format!("{} sandbox user {}", r, names.user(r)),
            StepId::NotAdmin(r) => format!("{} is not an administrator", names.user(r)),
            StepId::Group => format!("group {} holds both users", names.group()),
            StepId::Rights(r) => format!(
                "{} may not log on remotely, over the network, as a batch job or as a service",
                names.user(r)
            ),
            StepId::Hide(r) => format!("{} is hidden from the sign-in screen", names.user(r)),
            StepId::Rule(p) => format!("firewall rule {}", names.rule(p)),
            StepId::CloseFolders => "sandbox users cannot create files in the drive roots, \
                                     ProgramData, Public and the Windows temporary folders"
                .into(),
            StepId::DnsFilter => format!(
                "WFP filter: the DNS Client service refuses lookups from the offline users ({})",
                guid_text(WfpKeys::derive(names).filter)
            ),
            StepId::Record => "undo record".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Create,
    /// Exists and is kGraf's: bring it to the wanted state.
    Update,
    /// Check only; changes nothing.
    Verify,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: StepId,
    pub action: Action,
    /// What the undo record gains once this step succeeds.
    pub undo: Vec<UndoEntry>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error(
        "Windows Firewall is off for at least one profile, so the offline sandbox user's \
         network cannot be blocked; turn it on for the domain, private and public profiles"
    )]
    FirewallOff,
    #[error(
        "rules added to the local firewall policy do not take effect here (a group policy \
         overrides them), so the offline sandbox user's network cannot be blocked"
    )]
    FirewallPolicyIneffective,
    #[error(
        "a local user named {0} exists and kGraf did not make it for this account; rename or remove it, then run setup again"
    )]
    ForeignUser(String),
    #[error(
        "a local group named {0} exists and kGraf did not make it for this account; rename or remove it, then run setup again"
    )]
    ForeignGroup(String),
    #[error(
        "the sandbox user {0} is in the Administrators group; a sandbox user must not be an administrator. Remove it from the group, then run setup again"
    )]
    UserIsAdministrator(String),
    #[error("the undo record belongs to {found}, not to {expected}")]
    RecordOwner { expected: String, found: String },
    #[error(
        "the undo record lists {0}, which is not one of kGraf's names for this account; nothing was removed"
    )]
    ForeignEntry(String),
    #[error("the administrator step sent no password for {0}; nothing was stored. Run setup again")]
    PasswordMissing(String),
    #[error("the administrator step sent a password for {0}, which this setup did not ask for")]
    PasswordUnexpected(String),
}

/// The words after the elevated step's subcommand: what it does, how it
/// finds the owner's pipe, and which users it makes. `--slots` is always
/// written for a setup, so the elevated step makes the users the owner's
/// process expects. Without it, the step made only the two fixed users while
/// the owner's process asked for slot users.
pub fn elevated_params(
    names: &OwnerNames,
    setup: bool,
    pipe: &str,
    nonce: &str,
    owner_pid: u32,
) -> String {
    let mut params = format!(
        "--action {} --pipe {pipe} --nonce {nonce} --owner-pid {owner_pid} --owner-sid {}",
        if setup { "setup" } else { "remove" },
        names.owner_sid()
    );
    if setup {
        params.push_str(&format!(" --slots {}", names.slots()));
    }
    if let Some(tag) = names.tag() {
        params.push_str(&format!(" --tag {tag}"));
    }
    params
}

/// Whether the passwords the elevated step sent cover exactly the users
/// `names` asks for. Checked before anything is stored, so a setup that made
/// fewer users than asked fails loud instead of storing a partial set.
pub fn check_passwords(names: &OwnerNames, received: &[Role]) -> Result<(), PlanError> {
    let wanted = names.roles();
    if let Some(extra) = received.iter().find(|r| !wanted.contains(r)) {
        return Err(PlanError::PasswordUnexpected(names.user(*extra)));
    }
    match wanted.into_iter().find(|r| !received.contains(r)) {
        Some(missing) => Err(PlanError::PasswordMissing(names.user(missing))),
        None => Ok(()),
    }
}

/// The setup steps, in order.
pub fn plan_setup(names: &OwnerNames, observed: &Observed) -> Result<Vec<Step>, PlanError> {
    if observed.firewall.enabled_all_profiles == Some(false) {
        return Err(PlanError::FirewallOff);
    }
    if observed.firewall.local_policy_effective == Some(false) {
        return Err(PlanError::FirewallPolicyIneffective);
    }
    let mut steps = vec![Step {
        id: StepId::CheckFirewall,
        action: Action::Verify,
        undo: Vec::new(),
    }];
    for role in names.roles() {
        let user = names.user(role);
        let action = match observed.users.get(&user) {
            Some(f) if !names.owns_user_comment(role, &f.comment) => {
                return Err(PlanError::ForeignUser(user));
            }
            Some(f) if f.in_administrators => return Err(PlanError::UserIsAdministrator(user)),
            Some(_) => Action::Update,
            None => Action::Create,
        };
        steps.push(Step {
            id: StepId::User(role),
            action,
            undo: vec![UndoEntry::User(user)],
        });
    }
    for role in names.roles() {
        steps.push(Step {
            id: StepId::NotAdmin(role),
            action: Action::Verify,
            undo: Vec::new(),
        });
    }
    let group = names.group();
    let action = match &observed.group {
        Some(c) if !names.owns_group_comment(c) => return Err(PlanError::ForeignGroup(group)),
        Some(_) => Action::Update,
        None => Action::Create,
    };
    steps.push(Step {
        id: StepId::Group,
        action,
        undo: vec![UndoEntry::Group(group)],
    });
    for role in names.roles() {
        let user = names.user(role);
        steps.push(Step {
            id: StepId::Rights(role),
            action: Action::Update,
            undo: vec![UndoEntry::Rights {
                user: user.clone(),
                rights: DENIED_LOGON_RIGHTS
                    .iter()
                    .map(|r| (*r).to_owned())
                    .collect(),
            }],
        });
    }
    for role in names.roles() {
        let user = names.user(role);
        let action = if observed.hidden.contains(&user) {
            Action::Update
        } else {
            Action::Create
        };
        steps.push(Step {
            id: StepId::Hide(role),
            action,
            undo: vec![UndoEntry::Hidden(user)],
        });
    }
    for purpose in RulePurpose::ALL {
        let rule = names.rule(purpose);
        let action = if observed.rules.contains(&rule) {
            Action::Update
        } else {
            Action::Create
        };
        steps.push(Step {
            id: StepId::Rule(purpose),
            action,
            undo: vec![UndoEntry::Rule(rule)],
        });
    }
    // After the group: the deny entries name its SID. The step's undo list
    // is empty here because the group's SID is known only once the group
    // step has run; the step itself records one `Closed` entry per folder,
    // each as soon as that folder's entry is written.
    let all_closed =
        !observed.closed.is_empty() && observed.closed.values().all(|v| *v == Some(true));
    steps.push(Step {
        id: StepId::CloseFolders,
        action: if all_closed {
            Action::Update
        } else {
            Action::Create
        },
        undo: Vec::new(),
    });
    // After the users: the filter names the blocked users' SIDs.
    let keys = WfpKeys::derive(names);
    steps.push(Step {
        id: StepId::DnsFilter,
        action: if observed.wfp.contains(&keys.filter) {
            Action::Update
        } else {
            Action::Create
        },
        undo: keys
            .all()
            .into_iter()
            .map(|(object, key)| UndoEntry::Wfp { object, key })
            .collect(),
    });
    Ok(steps)
}

/// Whether `sid` may be the sandbox group's in a `Closed` undo entry: a
/// machine-local account SID, never a built-in group such as Users, and the
/// group's own SID while the group exists. Removal takes only the group's
/// own deny entry off, but this keeps an edited record from naming anyone
/// else.
fn is_group_sid(sid: &str, observed: &Observed) -> bool {
    sid.starts_with("S-1-5-21-")
        && observed
            .group_sid
            .as_deref()
            .is_none_or(|g| g.eq_ignore_ascii_case(sid))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveAction {
    Remove,
    /// Listed but already gone: nothing to do.
    AlreadyGone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveStep {
    pub entry: UndoEntry,
    pub action: RemoveAction,
}

impl RemoveStep {
    /// A single word for the pipe messages.
    pub fn key(&self) -> String {
        match &self.entry {
            UndoEntry::User(n) => format!("remove-user:{n}"),
            UndoEntry::Group(n) => format!("remove-group:{n}"),
            UndoEntry::Rights { user, .. } => format!("remove-rights:{user}"),
            UndoEntry::Hidden(n) => format!("remove-hidden:{n}"),
            UndoEntry::Rule(n) => format!("remove-rule:{n}"),
            UndoEntry::Closed { path, .. } => format!("remove-closed:{path}"),
            UndoEntry::Wfp { object, key } => {
                format!("remove-{}:{}", object.key(), guid_text(*key))
            }
        }
    }
}

fn entry_text(e: &UndoEntry) -> String {
    match e {
        UndoEntry::User(n) => format!("user {n}"),
        UndoEntry::Group(n) => format!("group {n}"),
        UndoEntry::Rights { user, .. } => format!("logon rights of {user}"),
        UndoEntry::Hidden(n) => format!("sign-in screen entry of {n}"),
        UndoEntry::Rule(n) => format!("firewall rule {n}"),
        UndoEntry::Closed { sid, path } => format!("the deny entry for {sid} on {path}"),
        UndoEntry::Wfp { object, key } => format!("{object} {}", guid_text(*key)),
    }
}

/// The removal steps for what the record lists, in an order that keeps each
/// step possible: rules and registry values first, logon rights while the
/// user's SID still resolves, the group, then the users.
pub fn plan_remove(
    names: &OwnerNames,
    record: &Record,
    observed: &Observed,
) -> Result<Vec<RemoveStep>, PlanError> {
    if record.owner_sid != names.owner_sid() || record.tag.as_deref() != names.tag() {
        return Err(PlanError::RecordOwner {
            expected: names.owner_sid().to_owned(),
            found: record.owner_sid.clone(),
        });
    }
    let rules = names.rules();
    let keys = WfpKeys::derive(names);
    for e in &record.entries {
        let ours = match e {
            UndoEntry::User(n) | UndoEntry::Hidden(n) => names.role_of(n).is_some(),
            UndoEntry::Rights { user, .. } => names.role_of(user).is_some(),
            UndoEntry::Group(n) => *n == names.group(),
            UndoEntry::Rule(n) => rules.contains(n),
            UndoEntry::Closed { sid, .. } => is_group_sid(sid, observed),
            UndoEntry::Wfp { object, key } => keys.get(*object) == *key,
        };
        if !ours {
            return Err(PlanError::ForeignEntry(entry_text(e)));
        }
    }
    // A deny entry goes before the group, while its SID still names it.
    let rank = |e: &UndoEntry| match e {
        // The filter before the sublayer and provider it names.
        UndoEntry::Wfp {
            object: WfpObject::Filter,
            ..
        } => -3,
        UndoEntry::Wfp {
            object: WfpObject::Sublayer,
            ..
        } => -2,
        UndoEntry::Wfp {
            object: WfpObject::Provider,
            ..
        } => -1,
        UndoEntry::Rule(_) => 0,
        UndoEntry::Hidden(_) | UndoEntry::Closed { .. } => 1,
        UndoEntry::Rights { .. } => 2,
        UndoEntry::Group(_) => 3,
        UndoEntry::User(_) => 4,
    };
    let mut entries = record.entries.clone();
    entries.sort_by_key(|e| rank(e));
    let mut steps = Vec::new();
    for entry in entries {
        let present = match &entry {
            UndoEntry::Rule(n) => observed.rules.contains(n),
            // Unreadable: try the removal, which treats "not found" as done.
            UndoEntry::Wfp { key, .. } => {
                observed.wfp_unreadable.is_some() || observed.wfp.contains(key)
            }
            UndoEntry::Hidden(n) => observed.hidden.contains(n),
            UndoEntry::Rights { user, .. } => observed.users.contains_key(user),
            // Whether the entry is still on the folder is read when it is
            // taken off: removing an absent entry changes nothing.
            UndoEntry::Closed { .. } => true,
            UndoEntry::Group(n) => match &observed.group {
                Some(c) if !names.owns_group_comment(c) => {
                    return Err(PlanError::ForeignGroup(n.clone()));
                }
                Some(_) => true,
                None => false,
            },
            UndoEntry::User(n) => match observed.users.get(n) {
                Some(f) => {
                    let role = names
                        .role_of(n)
                        .ok_or_else(|| PlanError::ForeignUser(n.clone()))?;
                    if !names.owns_user_comment(role, &f.comment) {
                        return Err(PlanError::ForeignUser(n.clone()));
                    }
                    true
                }
                None => false,
            },
        };
        steps.push(RemoveStep {
            entry,
            action: if present {
                RemoveAction::Remove
            } else {
                RemoveAction::AlreadyGone
            },
        });
    }
    Ok(steps)
}

// ---- status -----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemState {
    Present,
    Missing,
    /// Exists, but is not kGraf's or is not in the wanted state.
    Wrong(String),
    /// Could not be read.
    Unknown(String),
}

impl ItemState {
    fn word(&self) -> &'static str {
        match self {
            ItemState::Present => "present",
            ItemState::Missing => "MISSING",
            ItemState::Wrong(_) => "WRONG",
            ItemState::Unknown(_) => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    pub item: String,
    pub state: ItemState,
}

/// Whether a stored password still logs the user on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialCheck {
    LogonWorks,
    LogonFailed(String),
    NotChecked(String),
}

/// The owner's stored passwords.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credentials {
    Missing,
    Unreadable(String),
    /// The users the file holds, with the logon check of each.
    Stored(Vec<(String, CredentialCheck)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub lines: Vec<StatusLine>,
    /// Nothing is missing or wrong. A line that could not be read does not
    /// count against it, but is shown.
    pub ready: bool,
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for l in &self.lines {
            write!(f, "  {:<8} {}", l.state.word(), l.item)?;
            match &l.state {
                ItemState::Wrong(why) | ItemState::Unknown(why) => writeln!(f, ": {why}")?,
                _ => writeln!(f)?,
            }
        }
        if self.ready {
            write!(f, "The Windows sandbox user is set up.")
        } else {
            write!(
                f,
                "The Windows sandbox user is not fully set up: run `kgraf sandbox setup`."
            )
        }
    }
}

/// What exists and what is missing.
pub fn status_report(
    names: &OwnerNames,
    observed: &Observed,
    record: Result<Option<&Record>, String>,
    credentials: &Credentials,
) -> StatusReport {
    let mut lines = Vec::new();
    let mut push = |item: String, state: ItemState| lines.push(StatusLine { item, state });
    let tri = |v: Option<bool>, wrong: &str| match v {
        Some(true) => ItemState::Present,
        Some(false) => ItemState::Wrong(wrong.to_owned()),
        None => ItemState::Unknown("could not be read".into()),
    };
    push(
        "Windows Firewall on for every profile".into(),
        tri(
            observed.firewall.enabled_all_profiles,
            "off for at least one profile: the offline user's network is open",
        ),
    );
    push(
        "local firewall rules take effect".into(),
        tri(
            observed.firewall.local_policy_effective,
            "a group policy overrides local rules",
        ),
    );
    for role in names.roles() {
        let user = names.user(role);
        let state = match observed.users.get(&user) {
            None => ItemState::Missing,
            Some(f) if !names.owns_user_comment(role, &f.comment) => {
                ItemState::Wrong("not made by kGraf for this account".into())
            }
            Some(f) if f.in_administrators => {
                ItemState::Wrong("in the Administrators group".into())
            }
            Some(_) => ItemState::Present,
        };
        push(format!("{role} sandbox user {user}"), state);
    }
    let group = names.group();
    let state = match &observed.group {
        None => ItemState::Missing,
        Some(c) if !names.owns_group_comment(c) => {
            ItemState::Wrong("not made by kGraf for this account".into())
        }
        Some(_) => {
            let missing: Vec<String> = names
                .users()
                .into_iter()
                .filter(|u| !observed.group_members.contains(u))
                .collect();
            if missing.is_empty() {
                ItemState::Present
            } else {
                ItemState::Wrong(format!("does not hold {}", missing.join(", ")))
            }
        }
    };
    push(format!("group {group}"), state);
    for role in names.roles() {
        let user = names.user(role);
        let state = match observed.rights.get(&user) {
            _ if !observed.users.contains_key(&user) => ItemState::Missing,
            None | Some(None) => {
                ItemState::Unknown("reading logon rights needs administrator rights".into())
            }
            Some(Some(have)) => {
                let missing: Vec<&str> = DENIED_LOGON_RIGHTS
                    .iter()
                    .copied()
                    .filter(|r| !have.contains(*r))
                    .collect();
                if missing.is_empty() {
                    ItemState::Present
                } else {
                    ItemState::Wrong(format!("not denied: {}", missing.join(", ")))
                }
            }
        };
        push(format!("denied logon rights of {user}"), state);
    }
    for role in names.roles() {
        let user = names.user(role);
        let state = if observed.hidden.contains(&user) {
            ItemState::Present
        } else {
            ItemState::Missing
        };
        push(format!("{user} hidden from the sign-in screen"), state);
    }
    for rule in names.rules() {
        let state = if observed.rules.contains(&rule) {
            ItemState::Present
        } else {
            ItemState::Missing
        };
        push(format!("firewall rule {rule}"), state);
    }
    for (path, closed) in &observed.closed {
        let state = match closed {
            Some(true) => ItemState::Present,
            Some(false) => ItemState::Missing,
            None => ItemState::Unknown("its access list could not be read".into()),
        };
        push(
            format!("sandbox users cannot create files in {path}"),
            state,
        );
    }
    let filter = WfpKeys::derive(names).filter;
    let state = match &observed.wfp_unreadable {
        Some(why) => ItemState::Unknown(why.clone()),
        None if observed.wfp.contains(&filter) => ItemState::Present,
        None => ItemState::Missing,
    };
    // Present is not proved: only `kgraf sandbox selfcheck` shows the effect.
    push(
        format!(
            "DNS filter {} (WFP; the DNS Client service refuses the offline users; \
             `kgraf sandbox selfcheck` proves the effect)",
            guid_text(filter)
        ),
        state,
    );
    let state = match record {
        Ok(None) => ItemState::Missing,
        Ok(Some(r)) if r.owner_sid != names.owner_sid() || r.tag.as_deref() != names.tag() => {
            ItemState::Wrong(format!("belongs to {}", r.owner_sid))
        }
        Ok(Some(_)) => ItemState::Present,
        Err(e) => ItemState::Wrong(e),
    };
    push("undo record".into(), state);
    match credentials {
        Credentials::Missing => push("stored passwords".into(), ItemState::Missing),
        Credentials::Unreadable(why) => {
            push("stored passwords".into(), ItemState::Wrong(why.clone()))
        }
        Credentials::Stored(users) => {
            for role in names.roles() {
                let user = names.user(role);
                let state = match users.iter().find(|(u, _)| *u == user) {
                    None => ItemState::Missing,
                    Some((_, CredentialCheck::LogonWorks)) => ItemState::Present,
                    Some((_, CredentialCheck::LogonFailed(why))) => {
                        ItemState::Wrong(format!("the stored password does not log on: {why}"))
                    }
                    Some((_, CredentialCheck::NotChecked(why))) => ItemState::Unknown(why.clone()),
                };
                push(format!("stored password of {user}"), state);
            }
        }
    }
    let ready = lines
        .iter()
        .all(|l| matches!(l.state, ItemState::Present | ItemState::Unknown(_)));
    StatusReport { lines, ready }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1-2-3-1001";

    fn names() -> OwnerNames {
        OwnerNames::derive(OWNER, None).unwrap()
    }

    fn firewall_ok() -> FirewallFacts {
        FirewallFacts {
            enabled_all_profiles: Some(true),
            local_policy_effective: Some(true),
        }
    }

    /// The machine after a complete setup.
    fn full(n: &OwnerNames) -> Observed {
        let mut o = Observed {
            firewall: firewall_ok(),
            group: Some(n.group_comment()),
            ..Observed::default()
        };
        for (i, role) in Role::FIXED.into_iter().enumerate() {
            let user = n.user(role);
            o.users.insert(
                user.clone(),
                UserFacts {
                    sid: format!("S-1-5-21-1-2-3-{}", 1010 + i),
                    comment: n.user_comment(role),
                    in_administrators: false,
                },
            );
            o.group_members.insert(user.clone());
            o.hidden.insert(user.clone());
            o.rights.insert(
                user,
                Some(
                    DENIED_LOGON_RIGHTS
                        .iter()
                        .map(|r| (*r).to_owned())
                        .collect(),
                ),
            );
        }
        o.rules = n.rules().into_iter().collect();
        o.group_sid = Some("S-1-5-21-1-2-3-1100".into());
        for path in [r"C:\", r"C:\ProgramData"] {
            o.closed.insert(path.into(), Some(true));
        }
        o.wfp = WfpKeys::derive(n).all().iter().map(|(_, k)| *k).collect();
        o
    }

    fn record_from(steps: &[Step], n: &OwnerNames) -> Record {
        let mut r = Record::new(n.owner_sid(), n.tag());
        for s in steps {
            for u in &s.undo {
                r.add(u.clone());
            }
        }
        r
    }

    #[test]
    fn a_clean_machine_gets_every_step_as_a_create() {
        let n = names();
        let o = Observed {
            firewall: firewall_ok(),
            ..Observed::default()
        };
        let steps = plan_setup(&n, &o).unwrap();
        assert_eq!(
            steps[0].id,
            StepId::CheckFirewall,
            "the firewall is checked before any change"
        );
        let ids: Vec<StepId> = steps.iter().map(|s| s.id).collect();
        let pos = |id| ids.iter().position(|x| *x == id).unwrap();
        // Users exist before the steps that name them.
        for role in names().roles() {
            assert!(pos(StepId::User(role)) < pos(StepId::NotAdmin(role)));
            assert!(pos(StepId::User(role)) < pos(StepId::Group));
            assert!(pos(StepId::User(role)) < pos(StepId::Rights(role)));
            assert!(pos(StepId::User(role)) < pos(StepId::Hide(role)));
        }
        for p in RulePurpose::ALL {
            assert!(pos(StepId::User(Role::Offline)) < pos(StepId::Rule(p)));
        }
        for s in &steps {
            match s.id {
                StepId::CheckFirewall | StepId::NotAdmin(_) => assert_eq!(s.action, Action::Verify),
                StepId::Rights(_) => assert_eq!(s.action, Action::Update),
                _ => assert_eq!(s.action, Action::Create, "{:?}", s.id),
            }
        }
        // The undo list names every user, the group, both hidden entries,
        // both rights sets, every rule and the three WFP objects, and nothing
        // else.
        let r = record_from(&steps, &n);
        assert_eq!(r.entries.len(), 2 + 1 + 2 + 2 + RulePurpose::ALL.len() + 3);
        for user in n.users() {
            assert!(r.entries.contains(&UndoEntry::User(user.clone())));
            assert!(r.entries.contains(&UndoEntry::Hidden(user.clone())));
        }
        for rule in n.rules() {
            assert!(r.entries.contains(&UndoEntry::Rule(rule)));
        }
    }

    #[test]
    fn a_complete_machine_is_updated_not_recreated() {
        let n = names();
        let steps = plan_setup(&n, &full(&n)).unwrap();
        assert!(
            steps.iter().all(|s| s.action != Action::Create),
            "{steps:?}"
        );
        // Users are still in the plan: their passwords are reset.
        assert!(
            steps
                .iter()
                .any(|s| s.id == StepId::User(Role::Offline) && s.action == Action::Update)
        );
    }

    #[test]
    fn a_half_made_setup_creates_only_the_missing_parts() {
        let n = names();
        let mut o = full(&n);
        o.rules.remove(&n.rule(RulePurpose::BlockInbound));
        o.users.remove(&n.user(Role::Online));
        let steps = plan_setup(&n, &o).unwrap();
        let created: Vec<StepId> = steps
            .iter()
            .filter(|s| s.action == Action::Create)
            .map(|s| s.id)
            .collect();
        assert_eq!(
            created,
            vec![
                StepId::User(Role::Online),
                StepId::Rule(RulePurpose::BlockInbound)
            ]
        );
    }

    #[test]
    fn a_user_kgraf_did_not_make_is_refused() {
        let n = names();
        let mut o = full(&n);
        o.users.get_mut(&n.user(Role::Offline)).unwrap().comment = "someone's account".into();
        assert_eq!(
            plan_setup(&n, &o),
            Err(PlanError::ForeignUser(n.user(Role::Offline)))
        );
        let mut o = full(&n);
        o.group = Some("other".into());
        assert_eq!(plan_setup(&n, &o), Err(PlanError::ForeignGroup(n.group())));
    }

    #[test]
    fn an_administrator_sandbox_user_is_refused() {
        let n = names();
        let mut o = full(&n);
        o.users
            .get_mut(&n.user(Role::Online))
            .unwrap()
            .in_administrators = true;
        assert_eq!(
            plan_setup(&n, &o),
            Err(PlanError::UserIsAdministrator(n.user(Role::Online)))
        );
    }

    #[test]
    fn a_firewall_that_cannot_block_stops_the_plan_before_any_change() {
        let n = names();
        let mut o = full(&n);
        o.firewall.enabled_all_profiles = Some(false);
        assert_eq!(plan_setup(&n, &o), Err(PlanError::FirewallOff));
        o.firewall.enabled_all_profiles = Some(true);
        o.firewall.local_policy_effective = Some(false);
        assert_eq!(
            plan_setup(&n, &o),
            Err(PlanError::FirewallPolicyIneffective)
        );
        // Unknown is checked again by the step itself.
        o.firewall = FirewallFacts::default();
        assert!(plan_setup(&n, &o).is_ok());
    }

    #[test]
    fn step_keys_round_trip() {
        let n = names();
        let mut ids: Vec<StepId> = plan_setup(&n, &full(&n))
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        ids.push(StepId::Record);
        for id in ids {
            let key = id.key();
            assert!(!key.contains(' '), "{key}");
            assert_eq!(StepId::from_key(&key), Some(id));
            assert!(!id.describe(&n).is_empty());
        }
        for bad in ["", "user-", "user-admin", "rule-allow", "rights", "x"] {
            assert_eq!(StepId::from_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn remove_undoes_what_the_record_lists_in_a_safe_order() {
        let n = names();
        let o = full(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let steps = plan_remove(&n, &r, &o).unwrap();
        assert_eq!(steps.len(), r.entries.len());
        assert!(steps.iter().all(|s| s.action == RemoveAction::Remove));
        let rank = |e: &UndoEntry| match e {
            UndoEntry::Wfp {
                object: WfpObject::Filter,
                ..
            } => -3,
            UndoEntry::Wfp {
                object: WfpObject::Sublayer,
                ..
            } => -2,
            UndoEntry::Wfp {
                object: WfpObject::Provider,
                ..
            } => -1,
            UndoEntry::Rule(_) => 0,
            UndoEntry::Hidden(_) | UndoEntry::Closed { .. } => 1,
            UndoEntry::Rights { .. } => 2,
            UndoEntry::Group(_) => 3,
            UndoEntry::User(_) => 4,
        };
        let ranks: Vec<_> = steps.iter().map(|s| rank(&s.entry)).collect();
        let mut sorted = ranks.clone();
        sorted.sort();
        assert_eq!(ranks, sorted, "rights go before users, users last");
        // Only listed things: a record with one rule removes one rule.
        let mut one = Record::new(OWNER, None);
        one.add(UndoEntry::Rule(n.rule(RulePurpose::BlockOutbound)));
        let steps = plan_remove(&n, &one, &o).unwrap();
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn remove_twice_finds_everything_gone() {
        let n = names();
        let r = record_from(&plan_setup(&n, &full(&n)).unwrap(), &n);
        let empty = Observed::default();
        let steps = plan_remove(&n, &r, &empty).unwrap();
        assert!(steps.iter().all(|s| s.action == RemoveAction::AlreadyGone));
    }

    #[test]
    fn remove_refuses_entries_outside_the_owners_names() {
        let n = names();
        let o = full(&n);
        for foreign in [
            UndoEntry::User("Administrator".into()),
            UndoEntry::Group("Administrators".into()),
            UndoEntry::Rule("Core Networking - DNS (UDP-Out)".into()),
            UndoEntry::Hidden("Guest".into()),
            UndoEntry::Rights {
                user: "Administrator".into(),
                rights: vec![DENIED_LOGON_RIGHTS[0].into()],
            },
            // Another owner's sandbox user.
            UndoEntry::User(
                OwnerNames::derive("S-1-5-21-9-9-9-1001", None)
                    .unwrap()
                    .user(Role::Offline),
            ),
        ] {
            let mut r = Record::new(OWNER, None);
            r.add(UndoEntry::Rule(n.rule(RulePurpose::BlockOutbound)));
            r.add(foreign.clone());
            assert!(
                matches!(plan_remove(&n, &r, &o), Err(PlanError::ForeignEntry(_))),
                "{foreign:?}"
            );
        }
        let other = Record::new("S-1-5-21-9-9-9-1001", None);
        assert!(matches!(
            plan_remove(&n, &other, &o),
            Err(PlanError::RecordOwner { .. })
        ));
        let tagged = Record::new(OWNER, Some("wintest"));
        assert!(matches!(
            plan_remove(&n, &tagged, &o),
            Err(PlanError::RecordOwner { .. })
        ));
    }

    #[test]
    fn remove_refuses_a_user_that_changed_hands() {
        let n = names();
        let mut o = full(&n);
        o.users.get_mut(&n.user(Role::Offline)).unwrap().comment = "renamed into place".into();
        let mut r = Record::new(OWNER, None);
        r.add(UndoEntry::User(n.user(Role::Offline)));
        assert_eq!(
            plan_remove(&n, &r, &o),
            Err(PlanError::ForeignUser(n.user(Role::Offline)))
        );
    }

    fn all_stored(n: &OwnerNames) -> Credentials {
        Credentials::Stored(
            n.users()
                .into_iter()
                .map(|u| (u, CredentialCheck::LogonWorks))
                .collect(),
        )
    }

    #[test]
    fn status_of_a_complete_setup_is_ready() {
        let n = names();
        let o = full(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let report = status_report(&n, &o, Ok(Some(&r)), &all_stored(&n));
        assert!(report.ready, "{report}");
        assert!(
            report.lines.iter().all(|l| l.state == ItemState::Present),
            "{report}"
        );
        assert!(report.to_string().contains("is set up"));
    }

    #[test]
    fn status_names_each_missing_part() {
        let n = names();
        let mut o = full(&n);
        o.rules.remove(&n.rule(RulePurpose::BlockLoopbackTcp));
        o.hidden.remove(&n.user(Role::Online));
        let report = status_report(&n, &o, Ok(None), &Credentials::Missing);
        assert!(!report.ready);
        let missing: Vec<&str> = report
            .lines
            .iter()
            .filter(|l| l.state == ItemState::Missing)
            .map(|l| l.item.as_str())
            .collect();
        assert_eq!(missing.len(), 4, "{missing:?}");
        assert!(missing.iter().any(|m| m.contains("block-loopback-tcp")));
        assert!(missing.iter().any(|m| m.contains("undo record")));
        assert!(missing.iter().any(|m| m.contains("stored passwords")));
        assert!(report.to_string().contains("kgraf sandbox setup"));
    }

    #[test]
    fn status_on_a_clean_machine_is_all_missing() {
        let n = names();
        let report = status_report(&n, &Observed::default(), Ok(None), &Credentials::Missing);
        assert!(!report.ready);
        assert!(
            report
                .lines
                .iter()
                .filter(|l| !l.item.contains("irewall on") && !l.item.contains("take effect"))
                .all(|l| l.state == ItemState::Missing),
            "{report}"
        );
    }

    #[test]
    fn unreadable_rights_are_shown_but_do_not_block() {
        let n = names();
        let mut o = full(&n);
        for v in o.rights.values_mut() {
            *v = None;
        }
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let report = status_report(&n, &o, Ok(Some(&r)), &all_stored(&n));
        assert!(report.ready);
        assert!(
            report
                .lines
                .iter()
                .any(|l| matches!(l.state, ItemState::Unknown(_)))
        );
    }

    #[test]
    fn a_stale_password_or_a_wrong_user_is_not_ready() {
        let n = names();
        let o = full(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let stale = Credentials::Stored(vec![
            (
                n.user(Role::Offline),
                CredentialCheck::LogonFailed("1326".into()),
            ),
            (n.user(Role::Online), CredentialCheck::LogonWorks),
        ]);
        assert!(!status_report(&n, &o, Ok(Some(&r)), &stale).ready);
        let mut admin = o.clone();
        admin
            .users
            .get_mut(&n.user(Role::Offline))
            .unwrap()
            .in_administrators = true;
        assert!(!status_report(&n, &admin, Ok(Some(&r)), &all_stored(&n)).ready);
        let mut fw = o.clone();
        fw.firewall.enabled_all_profiles = Some(false);
        assert!(!status_report(&n, &fw, Ok(Some(&r)), &all_stored(&n)).ready);
    }

    /// The machine after a complete setup with `n`'s slot count.
    fn full_with_slots(n: &OwnerNames) -> Observed {
        let mut o = full(n);
        for (i, role) in n.roles().into_iter().enumerate() {
            let user = n.user(role);
            o.users.entry(user.clone()).or_insert(UserFacts {
                sid: format!("S-1-5-21-1-2-3-{}", 2000 + i),
                comment: n.user_comment(role),
                in_administrators: false,
            });
            o.group_members.insert(user.clone());
            o.hidden.insert(user.clone());
            o.rights.insert(
                user,
                Some(
                    DENIED_LOGON_RIGHTS
                        .iter()
                        .map(|r| (*r).to_owned())
                        .collect(),
                ),
            );
        }
        o
    }

    #[test]
    fn setup_status_and_remove_cover_every_pool_user() {
        let n = names().with_slots(3);
        let pool: Vec<Role> = (0..3)
            .map(Role::Slot)
            .chain((0..3).map(Role::OnlineSlot))
            .collect();
        // Setup makes, checks, hides and denies the logon rights of every
        // pool user.
        let clean = Observed {
            firewall: firewall_ok(),
            ..Observed::default()
        };
        let steps = plan_setup(&n, &clean).unwrap();
        for role in &pool {
            for id in [
                StepId::User(*role),
                StepId::NotAdmin(*role),
                StepId::Rights(*role),
                StepId::Hide(*role),
            ] {
                assert!(steps.iter().any(|s| s.id == id), "no step {id:?}");
            }
        }
        // Status lists each pool user, and is not ready when one is gone.
        let o = full_with_slots(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let report = status_report(&n, &o, Ok(Some(&r)), &all_stored(&n));
        assert!(report.ready, "{report}");
        for role in &pool {
            let user = n.user(*role);
            for item in [
                format!("{role} sandbox user {user}"),
                format!("stored password of {user}"),
                format!("{user} hidden from the sign-in screen"),
                format!("denied logon rights of {user}"),
            ] {
                assert!(
                    report.lines.iter().any(|l| l.item == item),
                    "no status line {item:?}"
                );
            }
            let mut gone = o.clone();
            gone.users.remove(&user);
            assert!(
                !status_report(&n, &gone, Ok(Some(&r)), &all_stored(&n)).ready,
                "status is ready without {user}"
            );
        }
        // The group must hold every pool user.
        let mut short = o.clone();
        short.group_members.remove(&n.user(Role::OnlineSlot(2)));
        assert!(!status_report(&n, &short, Ok(Some(&r)), &all_stored(&n)).ready);
        // Remove deletes every pool user the record lists, with names that
        // cover no slots: a removal does not know the slot count.
        let steps = plan_remove(&names(), &r, &o).unwrap();
        for role in &pool {
            let user = n.user(*role);
            assert!(
                steps
                    .iter()
                    .any(|s| s.entry == UndoEntry::User(user.clone())
                        && s.action == RemoveAction::Remove),
                "remove does not delete {user}"
            );
            assert!(
                steps
                    .iter()
                    .any(|s| matches!(&s.entry, UndoEntry::Rights { user: u, .. } if *u == user)),
                "remove keeps the logon rights of {user}"
            );
            assert!(
                steps
                    .iter()
                    .any(|s| s.entry == UndoEntry::Hidden(user.clone())),
                "remove keeps the sign-in screen entry of {user}"
            );
        }
    }

    #[test]
    fn the_elevated_step_is_told_how_many_slots_to_make() {
        let n = names().with_slots(4);
        let params = elevated_params(&n, true, "p", "n", 7);
        assert!(params.contains(" --slots 4"), "{params}");
        assert!(params.starts_with("--action setup "), "{params}");
        let tagged = OwnerNames::derive(OWNER, Some("t1")).unwrap();
        let params = elevated_params(&tagged, true, "p", "n", 7);
        assert!(
            params.contains(" --slots 0") && params.ends_with(" --tag t1"),
            "{params}"
        );
        assert!(!elevated_params(&n, false, "p", "n", 7).contains("--slots"));
    }

    #[test]
    fn a_setup_that_sent_fewer_passwords_than_users_is_refused() {
        let n = names().with_slots(2);
        assert_eq!(check_passwords(&n, &n.roles()), Ok(()));
        // What the elevated step sent when it was not told the slot count.
        assert_eq!(
            check_passwords(&n, &Role::FIXED),
            Err(PlanError::PasswordMissing(n.user(Role::Slot(0))))
        );
        let mut without_online = n.roles();
        without_online.retain(|r| *r != Role::OnlineSlot(1));
        assert_eq!(
            check_passwords(&n, &without_online),
            Err(PlanError::PasswordMissing(n.user(Role::OnlineSlot(1))))
        );
        let mut extra = n.roles();
        extra.push(Role::Slot(5));
        assert_eq!(
            check_passwords(&n, &extra),
            Err(PlanError::PasswordUnexpected(n.user(Role::Slot(5))))
        );
    }

    // ---- the DNS filter ----------------------------------------------------

    fn wfp_entries(n: &OwnerNames) -> Vec<UndoEntry> {
        WfpKeys::derive(n)
            .all()
            .into_iter()
            .map(|(object, key)| UndoEntry::Wfp { object, key })
            .collect()
    }

    #[test]
    fn setup_plans_the_dns_filter_after_the_blocked_users_and_records_its_keys() {
        let n = names().with_slots(3);
        let o = Observed {
            firewall: firewall_ok(),
            ..Observed::default()
        };
        let steps = plan_setup(&n, &o).unwrap();
        let ids: Vec<StepId> = steps.iter().map(|s| s.id).collect();
        let at = ids
            .iter()
            .position(|id| *id == StepId::DnsFilter)
            .expect("the DNS filter step is planned");
        assert_eq!(ids.iter().filter(|id| **id == StepId::DnsFilter).count(), 1);
        // The filter names the SIDs of the offline and slot users, so they
        // exist first.
        for role in n.roles().into_iter().filter(|r| r.blocked()) {
            let user_at = ids.iter().position(|id| *id == StepId::User(role)).unwrap();
            assert!(user_at < at, "{role} is made before the filter");
        }
        let step = &steps[at];
        assert_eq!(step.action, Action::Create);
        assert_eq!(step.undo, wfp_entries(&n), "provider, sublayer, filter");
        assert_eq!(StepId::from_key("dns-filter"), Some(StepId::DnsFilter));
        assert!(StepId::DnsFilter.describe(&n).contains("DNS"));
    }

    #[test]
    fn a_second_setup_refreshes_the_dns_filter_in_place() {
        let n = names();
        let first = plan_setup(
            &n,
            &Observed {
                firewall: firewall_ok(),
                ..Observed::default()
            },
        )
        .unwrap();
        let again = plan_setup(&n, &full(&n)).unwrap();
        let step = again.iter().find(|s| s.id == StepId::DnsFilter).unwrap();
        assert_eq!(step.action, Action::Update);
        // The same keys both times: a re-run replaces the filter, it never
        // adds a second one.
        let keys_of = |steps: &[Step]| {
            steps
                .iter()
                .find(|s| s.id == StepId::DnsFilter)
                .unwrap()
                .undo
                .clone()
        };
        assert_eq!(keys_of(&first), keys_of(&again));
        let mut r = record_from(&first, &n);
        let before = r.entries.len();
        for e in &step.undo {
            r.add(e.clone());
        }
        assert_eq!(r.entries.len(), before, "the record does not grow");
        // A filter whose provider survived but not the filter itself is made
        // again.
        let mut half = full(&n);
        half.wfp.remove(&WfpKeys::derive(&n).filter);
        let step = plan_setup(&n, &half)
            .unwrap()
            .into_iter()
            .find(|s| s.id == StepId::DnsFilter)
            .unwrap();
        assert_eq!(step.action, Action::Create);
    }

    #[test]
    fn remove_deletes_the_dns_filter_before_its_sublayer_and_provider() {
        let n = names();
        let o = full(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let steps = plan_remove(&n, &r, &o).unwrap();
        let wfp: Vec<(WfpObject, RemoveAction)> = steps
            .iter()
            .filter_map(|s| match s.entry {
                UndoEntry::Wfp { object, .. } => Some((object, s.action)),
                _ => None,
            })
            .collect();
        assert_eq!(
            wfp,
            vec![
                (WfpObject::Filter, RemoveAction::Remove),
                (WfpObject::Sublayer, RemoveAction::Remove),
                (WfpObject::Provider, RemoveAction::Remove),
            ]
        );
        // They go before anything else, while the users still exist.
        assert!(matches!(steps[0].entry, UndoEntry::Wfp { .. }));
        let key = steps[0].key();
        assert_eq!(
            key,
            format!(
                "remove-wfp-filter:{}",
                guid_text(WfpKeys::derive(&n).filter)
            )
        );
        // Gone already: nothing to do.
        let gone = plan_remove(&n, &r, &Observed::default()).unwrap();
        assert!(
            gone.iter()
                .filter(|s| matches!(s.entry, UndoEntry::Wfp { .. }))
                .all(|s| s.action == RemoveAction::AlreadyGone)
        );
        // Unreadable: the removal is tried.
        let unknown = Observed {
            wfp_unreadable: Some("access denied".into()),
            ..Observed::default()
        };
        assert!(
            plan_remove(&n, &r, &unknown)
                .unwrap()
                .iter()
                .filter(|s| matches!(s.entry, UndoEntry::Wfp { .. }))
                .all(|s| s.action == RemoveAction::Remove)
        );
    }

    #[test]
    fn remove_refuses_wfp_keys_that_are_not_this_owners() {
        let n = names();
        let o = full(&n);
        let other = OwnerNames::derive("S-1-5-21-9-9-9-1001", None).unwrap();
        let tagged = OwnerNames::derive(OWNER, Some("wintest")).unwrap();
        for foreign in [
            wfp_entries(&other)[2].clone(),
            wfp_entries(&tagged)[0].clone(),
            UndoEntry::Wfp {
                object: WfpObject::Filter,
                key: WfpKeys::derive(&n).provider,
            },
            UndoEntry::Wfp {
                object: WfpObject::Filter,
                key: 0x75a89dda_95e4_40f3_adc7_7688a9c847e1,
            },
        ] {
            let mut r = Record::new(OWNER, None);
            r.add(foreign.clone());
            assert!(
                matches!(plan_remove(&n, &r, &o), Err(PlanError::ForeignEntry(_))),
                "{foreign:?}"
            );
        }
    }

    #[test]
    fn status_reports_the_dns_filter() {
        let n = names();
        let line = |o: &Observed| {
            status_report(&n, o, Ok(None), &Credentials::Missing)
                .lines
                .into_iter()
                .find(|l| l.item.contains("DNS"))
                .expect("a DNS filter line")
        };
        let o = full(&n);
        assert_eq!(line(&o).state, ItemState::Present);
        assert!(
            line(&o)
                .item
                .contains(&guid_text(WfpKeys::derive(&n).filter))
        );
        let mut missing = o.clone();
        missing.wfp.clear();
        assert_eq!(line(&missing).state, ItemState::Missing);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        assert!(!status_report(&n, &missing, Ok(Some(&r)), &all_stored(&n)).ready);
        let mut unread = o.clone();
        unread.wfp.clear();
        unread.wfp_unreadable = Some("needs administrator rights".into());
        assert!(matches!(line(&unread).state, ItemState::Unknown(_)));
    }
}

#[cfg(test)]
mod closed_folder_tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1-2-3-1001";
    const GROUP_SID: &str = "S-1-5-21-1-2-3-1100";

    fn names() -> OwnerNames {
        OwnerNames::derive(OWNER, None).unwrap()
    }

    fn closed(sid: &str, path: &str) -> UndoEntry {
        UndoEntry::Closed {
            sid: sid.into(),
            path: path.into(),
        }
    }

    #[test]
    fn setup_closes_the_shared_folders_after_the_group_exists() {
        let n = names();
        let mut o = Observed {
            firewall: FirewallFacts {
                enabled_all_profiles: Some(true),
                local_policy_effective: Some(true),
            },
            ..Observed::default()
        };
        o.closed.insert(r"C:\".into(), Some(false));
        let steps = plan_setup(&n, &o).unwrap();
        let pos = |id| steps.iter().position(|s| s.id == id).unwrap();
        assert!(pos(StepId::Group) < pos(StepId::CloseFolders));
        assert_eq!(steps[pos(StepId::CloseFolders)].action, Action::Create);
        assert_eq!(
            StepId::from_key(&StepId::CloseFolders.key()),
            Some(StepId::CloseFolders)
        );
        o.closed.insert(r"C:\".into(), Some(true));
        let steps = plan_setup(&n, &o).unwrap();
        let step = steps.iter().find(|s| s.id == StepId::CloseFolders).unwrap();
        assert_eq!(step.action, Action::Update);
        // A folder whose list could not be read is written again.
        o.closed.insert(r"D:\".into(), None);
        let steps = plan_setup(&n, &o).unwrap();
        let step = steps.iter().find(|s| s.id == StepId::CloseFolders).unwrap();
        assert_eq!(step.action, Action::Create);
    }

    #[test]
    fn remove_takes_the_deny_entries_off_before_the_group_goes() {
        let n = names();
        let o = Observed {
            group: Some(n.group_comment()),
            group_sid: Some(GROUP_SID.into()),
            ..Observed::default()
        };
        let mut r = Record::new(OWNER, None);
        r.add(UndoEntry::Group(n.group()));
        r.add(closed(GROUP_SID, r"C:\ProgramData"));
        let steps = plan_remove(&n, &r, &o).unwrap();
        let order: Vec<&UndoEntry> = steps.iter().map(|s| &s.entry).collect();
        assert_eq!(
            order,
            vec![
                &closed(GROUP_SID, r"C:\ProgramData"),
                &UndoEntry::Group(n.group())
            ]
        );
        assert!(steps.iter().all(|s| s.action == RemoveAction::Remove));
    }

    #[test]
    fn remove_refuses_a_deny_entry_for_anyone_but_the_group() {
        let n = names();
        let o = Observed {
            group: Some(n.group_comment()),
            group_sid: Some(GROUP_SID.into()),
            ..Observed::default()
        };
        // Users, Everyone, SYSTEM, another local account.
        for sid in ["S-1-5-32-545", "S-1-1-0", "S-1-5-18", "S-1-5-21-1-2-3-1001"] {
            let mut r = Record::new(OWNER, None);
            r.add(closed(sid, r"C:\"));
            assert!(
                matches!(plan_remove(&n, &r, &o), Err(PlanError::ForeignEntry(_))),
                "{sid}"
            );
        }
        // With the group gone, a local SID is taken: the entry can only be
        // the group's, whose SID no longer resolves. A built-in one is not.
        let gone = Observed::default();
        let mut r = Record::new(OWNER, None);
        r.add(closed(GROUP_SID, r"C:\"));
        assert!(plan_remove(&n, &r, &gone).is_ok());
        r.add(closed("S-1-5-32-545", r"C:\"));
        assert!(plan_remove(&n, &r, &gone).is_err());
    }

    #[test]
    fn status_names_each_folder_left_open() {
        let n = names();
        let mut o = Observed::default();
        o.closed.insert(r"C:\".into(), Some(true));
        o.closed.insert(r"C:\ProgramData".into(), Some(false));
        o.closed.insert(r"D:\".into(), None);
        let report = status_report(&n, &o, Ok(None), &Credentials::Missing);
        let line = |p: &str| {
            report
                .lines
                .iter()
                .find(|l| l.item == format!("sandbox users cannot create files in {p}"))
                .map(|l| l.state.clone())
                .unwrap()
        };
        assert_eq!(line(r"C:\"), ItemState::Present);
        assert_eq!(line(r"C:\ProgramData"), ItemState::Missing);
        assert!(matches!(line(r"D:\"), ItemState::Unknown(_)));
    }
}
