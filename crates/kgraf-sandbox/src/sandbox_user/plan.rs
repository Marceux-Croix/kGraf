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
            StepId::Record => "record".into(),
        }
    }

    pub fn from_key(key: &str) -> Option<StepId> {
        let role = |s: &str| Role::from_key(s);
        Some(match key {
            "check-firewall" => StepId::CheckFirewall,
            "group" => StepId::Group,
            "record" => StepId::Record,
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
    for role in Role::ALL {
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
    for role in Role::ALL {
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
    for role in Role::ALL {
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
    for role in Role::ALL {
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
    Ok(steps)
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
    for e in &record.entries {
        let ours = match e {
            UndoEntry::User(n) | UndoEntry::Hidden(n) => names.role_of(n).is_some(),
            UndoEntry::Rights { user, .. } => names.role_of(user).is_some(),
            UndoEntry::Group(n) => *n == names.group(),
            UndoEntry::Rule(n) => rules.contains(n),
        };
        if !ours {
            return Err(PlanError::ForeignEntry(entry_text(e)));
        }
    }
    let rank = |e: &UndoEntry| match e {
        UndoEntry::Rule(_) => 0,
        UndoEntry::Hidden(_) => 1,
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
            UndoEntry::Hidden(n) => observed.hidden.contains(n),
            UndoEntry::Rights { user, .. } => observed.users.contains_key(user),
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
    for role in Role::ALL {
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
    for role in Role::ALL {
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
    for role in Role::ALL {
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
            for role in Role::ALL {
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
        for (i, role) in Role::ALL.into_iter().enumerate() {
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
        for role in Role::ALL {
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
        // both rights sets and every rule, and nothing else.
        let r = record_from(&steps, &n);
        assert_eq!(r.entries.len(), 2 + 1 + 2 + 2 + RulePurpose::ALL.len());
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
            UndoEntry::Rule(_) => 0,
            UndoEntry::Hidden(_) => 1,
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
}
