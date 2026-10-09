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
use super::handshake::StepOutcome;
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
    /// ([`super::acl::closed_folders`]) that exist, each with whether the
    /// group's deny entry is on it; `None` when that could not be read.
    pub closed: BTreeMap<String, Option<bool>>,
    /// Every folder setup closes on this machine
    /// ([`super::acl::closed_folders`]), whether it exists or not. Removal
    /// takes a `closed` record line's deny entry off only on a folder in
    /// this set, so a line whose path was edited cannot make it change the
    /// access list of any other folder.
    pub closable: BTreeSet<String>,
    /// The keys of kGraf's WFP objects (provider, sublayer, DNS filter) that
    /// exist.
    pub wfp: BTreeSet<u128>,
    /// The filter keys that exist but that the Base Filtering Engine has
    /// disabled: present, and not effective.
    pub wfp_disabled: BTreeSet<u128>,
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
    /// Add or refresh the WFP filters that keep the blocked users from the
    /// DNS Client service and from loopback ports other than kGraf's egress
    /// proxy ([`super::wfp`]). The key stays `dns-filter`, the step's name
    /// before the loopback filters joined it.
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
                "WFP filters meant to refuse the offline users' DNS lookups and loopback \
                 connections outside kGraf's egress proxy ports ({})",
                WfpKeys::derive(names)
                    .filters()
                    .iter()
                    .map(|(_, k)| guid_text(*k))
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
            StepId::Record => "undo record".into(),
        }
    }
}

/// What a failure of `id` leaves on the machine, as a clause the setup's
/// error carries. Empty where the failure leaves nothing that matters.
///
/// A filter that Windows refuses is not a failure of the step
/// ([`dns_step_report`]); the step fails only when kGraf cannot name the
/// users the filters match.
///
/// A first setup that fails stores no passwords, so no command runs as a
/// sandbox user. A re-run is different: the credential store from the earlier
/// setup stays, and the launch path does not read the WFP keys, so commands
/// keep running as the sandbox user with whatever filter the machine has. The
/// launch path cannot check the filters itself: reading a WFP object can need
/// administrator rights, and a command's launch is a standard user's. So the
/// failure says it here, where it happens.
pub fn failure_leaves(id: StepId) -> &'static str {
    match id {
        StepId::DnsFilter => {
            "the sandbox users and the stored passwords of an earlier setup stay in place, so \
             commands still run as a sandbox user without these filters: a name lookup can \
             leave the machine and a loopback port outside kGraf's egress proxy can be \
             reached; run `kgraf sandbox status` for the filter lines, \
             `kgraf sandbox selfcheck` for the effect, and `kgraf sandbox remove` to take the \
             users off the machine"
        }
        _ => "",
    }
}

/// What the WFP filter step did with one filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterOutcome {
    Added,
    /// Not added, with why: Windows' status, and what the probe filters
    /// showed when the layer refused it.
    NotAdded(String),
}

/// What the WFP filter step reports: the step's outcome, and each filter
/// that was not added, with why, in the order the filters are added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsStepReport {
    pub outcome: StepOutcome,
    pub refused: Vec<(WfpObject, String)>,
}

/// The WFP filter step's report from what happened to each filter.
///
/// A filter Windows refuses does not fail setup: the gap is kept and
/// reported as a failed line, never a pass. The users, the firewall rules
/// and the folder denies are the main confinement, and a setup that stopped
/// at this step stored no passwords, so no command ran as a sandbox user at
/// all. The step's outcome is then `partial`, each refused filter is listed
/// with why, and setup ends with [`dns_notice`]. A filter missing from
/// `results` is listed as not tried, so a result list that lost one never
/// reads as a full set. A refused loopback filter is a gap the same way: the
/// selfcheck's direct connection to its loopback origin then fails the
/// check, so the gap is never silent.
pub fn dns_step_report(action: Action, results: &[(WfpObject, FilterOutcome)]) -> DnsStepReport {
    let refused: Vec<(WfpObject, String)> = WfpObject::FILTERS
        .into_iter()
        .filter_map(|object| match results.iter().find(|(o, _)| *o == object) {
            Some((_, FilterOutcome::Added)) => None,
            Some((_, FilterOutcome::NotAdded(why))) => Some((object, why.clone())),
            None => Some((object, "setup did not try to add it".to_owned())),
        })
        .collect();
    let outcome = if !refused.is_empty() {
        StepOutcome::Partial
    } else if action == Action::Update {
        StepOutcome::Updated
    } else {
        StepOutcome::Created
    };
    DnsStepReport { outcome, refused }
}

/// Whether a refused filter leaves a lookup on this machine without a
/// filter: one of the name resolution cache layers. The RPC layer sees a
/// remote caller only.
fn leaves_local_lookups_open(object: WfpObject) -> bool {
    matches!(
        object,
        WfpObject::NameCacheFilterV4 | WfpObject::NameCacheFilterV6
    )
}

/// The notice setup ends with when a WFP filter was not added, or `None`
/// when every filter was: one paragraph for the loopback filters and one for
/// the DNS filters, each only when one of its kind was refused.
pub fn dns_notice(refused: &[(WfpObject, String)]) -> Option<String> {
    let loopback: Vec<&(WfpObject, String)> = refused
        .iter()
        .filter(|(o, _)| o.is_loopback_filter())
        .collect();
    let dns: Vec<(WfpObject, String)> = refused
        .iter()
        .filter(|(o, _)| !o.is_loopback_filter())
        .cloned()
        .collect();
    let loopback_notice = (!loopback.is_empty()).then(|| {
        let list = loopback
            .iter()
            .map(|(object, why)| {
                format!(
                    "the filter blocking {} on {} ({why})",
                    object.loopback_words(),
                    object.layer_words()
                )
            })
            .collect::<Vec<String>>()
            .join("; ");
        format!(
            "Loopback connections are not confined on this machine. Windows did not add \
             {list}. Windows Firewall rules do not act on loopback traffic, so a sandbox user \
             without the network can reach a service listening on this machine. \
             `kgraf sandbox status` shows the gap as FAILED, and `kgraf sandbox selfcheck` \
             fails on its direct connection."
        )
    });
    let dns_notice = dns_only_notice(&dns);
    match (loopback_notice, dns_notice) {
        (None, None) => None,
        (Some(l), None) => Some(l),
        (None, Some(d)) => Some(d),
        (Some(l), Some(d)) => Some(format!("{l}\n{d}")),
    }
}

/// [`dns_notice`] for the DNS filters alone.
fn dns_only_notice(refused: &[(WfpObject, String)]) -> Option<String> {
    if refused.is_empty() {
        return None;
    }
    let list = refused
        .iter()
        .map(|(object, why)| format!("the filter on {} ({why})", object.layer_words()))
        .collect::<Vec<String>>()
        .join("; ");
    Some(
        if refused.iter().any(|(o, _)| leaves_local_lookups_open(*o)) {
            format!(
                "DNS is not confined on this machine. Windows did not add {list}. A name lookup \
                 by a sandbox user without the network goes through the DNS Client service and \
                 can leave the machine. The sandbox users, the firewall rules and the folder \
                 denies are in place. `kgraf sandbox status` shows the gap as FAILED, and \
                 `kgraf sandbox selfcheck` prints a `dns:` line that is not a pass."
            )
        } else {
            format!(
                "Windows did not add {list}. That filter covers remote callers of the DNS \
                 Client service only; a lookup on this machine is judged by \
                 `kgraf sandbox selfcheck`. `kgraf sandbox status` shows the gap as FAILED."
            )
        },
    )
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
    #[error(
        "the undo record lists {0}, which this removal did not read from the machine; nothing \
         was removed"
    )]
    NotRead(String),
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
    // After the users: the filters name the blocked users' SIDs.
    let keys = WfpKeys::derive(names);
    steps.push(Step {
        id: StepId::DnsFilter,
        // Update only when every filter is already there and runs: one
        // missing or disabled filter means the layer it covers is open, so
        // the step is a Create.
        action: if keys
            .filters()
            .iter()
            .all(|(_, k)| observed.wfp.contains(k) && !observed.wfp_disabled.contains(k))
        {
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

/// Whether `sid` is an account of the same authority as the owner, with a
/// relative identifier an account database hands out (1,000 and up).
///
/// A prefix of `S-1-5-21-` does not say this: every domain SID starts with
/// it too, so `Domain Users` passed that test. The owner's own SID names the
/// authority that also made the sandbox group, so its prefix is the bound.
fn same_authority(sid: &str, owner_sid: &str) -> bool {
    let Some((authority, _)) = owner_sid.rsplit_once('-') else {
        return false;
    };
    sid.len() > authority.len()
        && sid[..authority.len()].eq_ignore_ascii_case(authority)
        && sid[authority.len()..]
            .strip_prefix('-')
            .and_then(|rid| rid.parse::<u32>().ok())
            .is_some_and(|rid| rid >= 1000)
}

/// Why removal may not take the deny entry of a `closed` record line off, or
/// `None` when it may.
///
/// Two bounds, because the line is read from a file and a path or a SID in
/// it could have been edited (which needs administrator rights, so this is
/// the last fence, not the first):
///
/// - the folder must be one setup closes on this machine, so an edited path
///   cannot send the removal at another folder;
/// - the SID must be the sandbox group's own, or an account of the owner's
///   own authority, not the owner itself, that no longer resolves, which is
///   a sandbox group an earlier setup made and that was deleted since. A SID
///   that still resolves names a live principal, and kGraf never takes an
///   entry off on its behalf.
///
/// `resolves` answers whether a SID still names an account: `Some(false)`
/// for an account that is gone, `None` when it could not be read, which
/// counts against removal.
fn closed_entry_refusal(
    sid: &str,
    path: &str,
    names: &OwnerNames,
    observed: &Observed,
    resolves: &dyn Fn(&str) -> Option<bool>,
) -> Option<String> {
    if !observed
        .closable
        .iter()
        .any(|p| p.eq_ignore_ascii_case(path))
    {
        return Some(format!(
            "{path} is not one of the folders setup closes on this machine"
        ));
    }
    if observed
        .group_sid
        .as_deref()
        .is_some_and(|g| g.eq_ignore_ascii_case(sid))
    {
        return None;
    }
    if sid.eq_ignore_ascii_case(names.owner_sid()) {
        return Some(format!(
            "{sid} is the owner's own account, which is never the sandbox group"
        ));
    }
    if !same_authority(sid, names.owner_sid()) {
        return Some(format!(
            "{sid} is not an account of the same authority as the owner"
        ));
    }
    match resolves(sid) {
        Some(false) => None,
        Some(true) => Some(format!(
            "{sid} still names an account, so it is not a sandbox group of an earlier setup"
        )),
        None => Some(format!(
            "whether {sid} still names an account could not be read"
        )),
    }
}

/// The `closed` entries of the record that name a group other than the one
/// the current setup made, and that setup may undo itself: the folder is one
/// this machine closes and the SID is of the owner's own authority.
///
/// Setup opens these folders again and drops the entries before it writes
/// its own, so a generation left behind by a group that was deleted outside
/// kGraf does not pile up. Without this, each setup added ten more entries
/// and ten more record lines, and `remove` could never finish.
pub fn stale_closed_entries(
    names: &OwnerNames,
    record: &Record,
    observed: &Observed,
    resolves: &dyn Fn(&str) -> Option<bool>,
) -> Vec<UndoEntry> {
    let Some(group) = observed.group_sid.as_deref() else {
        return Vec::new();
    };
    record
        .entries
        .iter()
        .filter(|e| match e {
            UndoEntry::Closed { sid, path, .. } => {
                // The same bounds as a removal: the folder is one this
                // machine closes, and the SID names a group of the owner's
                // own authority that no longer resolves. An entry for a
                // group that still exists is another owner's, and setup
                // leaves it where it is.
                !sid.eq_ignore_ascii_case(group)
                    && closed_entry_refusal(sid, path, names, observed, resolves).is_none()
            }
            _ => false,
        })
        .cloned()
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveAction {
    Remove,
    /// Listed but already gone: nothing to do.
    AlreadyGone,
    /// Listed, and this removal may not undo it ([`RemoveStep::note`] says
    /// why). The entry stays in the undo record, and the rest of the plan is
    /// carried out: one entry kGraf cannot judge must not stop a removal
    /// from taking the users, the group, the rights and the rules away.
    Kept,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveStep {
    pub entry: UndoEntry,
    pub action: RemoveAction,
    /// Why the entry is kept, for [`RemoveAction::Kept`] alone.
    pub note: Option<String>,
}

impl RemoveStep {
    /// The key of this step's pipe messages ([`undo_key`]).
    pub fn key(&self) -> String {
        undo_key(&self.entry)
    }
}

/// The key of a removal step's pipe messages. It carries a user, group or
/// rule name, a GUID or a folder path as they are; the pipe escapes what one
/// word cannot hold ([`super::handshake::escape_key`]).
pub fn undo_key(e: &UndoEntry) -> String {
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

/// How `kgraf sandbox remove` ends on the owner's side. `left` names each
/// place the removal could not clear before the users went (a folder whose
/// sandbox grant could not be revoked, with the reason; a slot table that
/// could not be read), and `elevated` is the elevated step's error, if it
/// failed. Neither stops the removal: every folder is tried and the elevated
/// step runs whatever the folders did. The removal fails at the end, naming
/// all of them, so a folder that kept its grant is never reported as a
/// finished removal.
pub fn removal_outcome(left: &[String], elevated: Option<&str>) -> Result<(), String> {
    if left.is_empty() && elevated.is_none() {
        return Ok(());
    }
    let mut parts = Vec::new();
    if let Some(e) = elevated {
        parts.push(e.to_owned());
    }
    if !left.is_empty() {
        parts.push(format!(
            "{} place(s) kept an access-list entry for a sandbox user: {}",
            left.len(),
            left.join("; ")
        ));
    }
    Err(parts.join("; "))
}

fn entry_text(e: &UndoEntry) -> String {
    match e {
        UndoEntry::User(n) => format!("user {n}"),
        UndoEntry::Group(n) => format!("group {n}"),
        UndoEntry::Rights { user, .. } => format!("logon rights of {user}"),
        UndoEntry::Hidden(n) => format!("sign-in screen entry of {n}"),
        UndoEntry::Rule(n) => format!("firewall rule {n}"),
        UndoEntry::Closed { sid, path, .. } => format!("the deny entry for {sid} on {path}"),
        UndoEntry::Wfp { object, key } => format!("{object} {}", guid_text(*key)),
    }
}

/// The removal steps for what the record lists, in an order that keeps each
/// step possible: rules and registry values first, the folder deny entries
/// and the hidden values, logon rights while the user's SID still resolves,
/// the group, then the users, and the WFP objects last of all, each filter
/// before the sublayer and the provider it names.
pub fn plan_remove(
    names: &OwnerNames,
    record: &Record,
    observed: &Observed,
    resolves: &dyn Fn(&str) -> Option<bool>,
) -> Result<Vec<RemoveStep>, PlanError> {
    if record.owner_sid != names.owner_sid() || record.tag.as_deref() != names.tag() {
        return Err(PlanError::RecordOwner {
            expected: names.owner_sid().to_owned(),
            found: record.owner_sid.clone(),
        });
    }
    let rules = names.rules();
    let keys = WfpKeys::derive(names);
    // Why each `closed` entry may not be undone, by entry. A refusal here is
    // never a refusal of the whole plan: a deny entry whose group was deleted
    // outside kGraf would otherwise block every removal for good, leaving the
    // users, the group, the rights, the rules and every other deny entry in
    // place with no way out but `icacls` by hand.
    let kept: BTreeMap<&UndoEntry, String> = record
        .entries
        .iter()
        .filter_map(|e| match e {
            UndoEntry::Closed { sid, path, .. } => {
                closed_entry_refusal(sid, path, names, observed, resolves).map(|why| (e, why))
            }
            _ => None,
        })
        .collect();
    for e in &record.entries {
        let ours = match e {
            UndoEntry::User(n) | UndoEntry::Hidden(n) => names.role_of(n).is_some(),
            UndoEntry::Rights { user, .. } => names.role_of(user).is_some(),
            UndoEntry::Group(n) => *n == names.group(),
            UndoEntry::Rule(n) => rules.contains(n),
            // Judged above, with its own reason in the error.
            UndoEntry::Closed { .. } => true,
            UndoEntry::Wfp { object, key } => keys.get(*object) == *key,
        };
        if !ours {
            return Err(PlanError::ForeignEntry(entry_text(e)));
        }
        // A user the caller did not read is absent from `observed`, and an
        // absent user is planned as already gone, which drops its record
        // line with the user still on the machine. The caller must read
        // every role the record names.
        let user = match e {
            UndoEntry::User(n) | UndoEntry::Hidden(n) => Some(n),
            UndoEntry::Rights { user, .. } => Some(user),
            _ => None,
        };
        if user
            .and_then(|n| names.role_of(n))
            .is_some_and(|role| !names.roles().contains(&role))
        {
            return Err(PlanError::NotRead(entry_text(e)));
        }
    }
    // A deny entry goes before the group, while its SID still names it.
    //
    // The WFP objects go last, after the accounts. Removal used to stop at
    // the first step that failed, and now goes on past it (the elevated
    // step's `run_remove`); the order still puts what matters most first. A
    // block filter that will not go is fail-safe: it
    // blocks name lookups for users that no longer exist, which is nothing.
    // An account that will not go is not fail-safe. A stopped Base Filtering
    // Engine, a firewall service that is off, or a sublayer another object
    // still names would otherwise leave the sandbox users, the group, the
    // denied logon rights, the firewall rules and the folder deny entries on
    // the machine. Nothing in the removal of a filter needs the users: its
    // user condition is a security descriptor of SIDs, which deletes with or
    // without them.
    let rank = |e: &UndoEntry| match e {
        UndoEntry::Rule(_) => 0,
        UndoEntry::Hidden(_) | UndoEntry::Closed { .. } => 1,
        UndoEntry::Rights { .. } => 2,
        UndoEntry::Group(_) => 3,
        UndoEntry::User(_) => 4,
        // Each filter before the sublayer and the provider it names: a
        // provider or sublayer an object still names cannot be deleted.
        UndoEntry::Wfp { object, .. } => match object {
            WfpObject::Sublayer => 6,
            WfpObject::Provider => 7,
            _ => 5,
        },
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
        let note = kept.get(&entry).cloned();
        steps.push(RemoveStep {
            entry,
            action: match (&note, present) {
                (Some(_), _) => RemoveAction::Kept,
                (None, true) => RemoveAction::Remove,
                (None, false) => RemoveAction::AlreadyGone,
            },
            note,
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
    /// Windows refused it at setup, so running setup again does not add it:
    /// a gap, shown as a failed line.
    Failed(String),
}

impl ItemState {
    fn word(&self) -> &'static str {
        match self {
            ItemState::Present => "present",
            ItemState::Missing => "MISSING",
            ItemState::Wrong(_) => "WRONG",
            ItemState::Unknown(_) => "unknown",
            ItemState::Failed(_) => "FAILED",
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
    /// Nothing is missing, wrong or failed. A line that could not be read
    /// does not count against it, but is shown.
    pub ready: bool,
}

impl StatusReport {
    /// What the failed lines leave open, in words: DNS, loopback connections,
    /// or both. Empty when no line failed.
    pub fn refused_gaps(&self) -> String {
        let failed = |prefix: &str| {
            self.lines
                .iter()
                .any(|l| matches!(l.state, ItemState::Failed(_)) && l.item.starts_with(prefix))
        };
        let mut gaps = Vec::new();
        if failed(DNS_FILTER_ITEM) {
            gaps.push("DNS");
        }
        if failed(LOOPBACK_FILTER_ITEM) {
            gaps.push("loopback connections");
        }
        gaps.join(" and ")
    }

    /// Not ready, and only because of lines Windows refused at setup (the
    /// WFP filters): running setup again would not change it.
    pub fn only_refused_gaps(&self) -> bool {
        !self.ready
            && self.lines.iter().all(|l| {
                matches!(
                    l.state,
                    ItemState::Present | ItemState::Unknown(_) | ItemState::Failed(_)
                )
            })
    }
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for l in &self.lines {
            write!(f, "  {:<8} {}", l.state.word(), l.item)?;
            match &l.state {
                ItemState::Wrong(why) | ItemState::Unknown(why) | ItemState::Failed(why) => {
                    writeln!(f, ": {why}")?
                }
                _ => writeln!(f)?,
            }
        }
        if self.ready {
            write!(f, "The Windows sandbox user is set up.")
        } else if self.only_refused_gaps() {
            write!(
                f,
                "The Windows sandbox user is set up, but {} not confined on this machine: \
                 Windows refused a WFP filter at setup, and running setup again does not \
                 change that.",
                refused_subject(&self.refused_gaps())
            )
        } else {
            write!(
                f,
                "The Windows sandbox user is not fully set up: run `kgraf sandbox setup`."
            )
        }
    }
}

/// The item of a DNS filter's status line starts with this.
const DNS_FILTER_ITEM: &str = "DNS filter on ";
/// The item of a loopback filter's status line starts with this.
const LOOPBACK_FILTER_ITEM: &str = "loopback filter on ";

/// `gaps` ([`StatusReport::refused_gaps`]) as the subject of a sentence:
/// "DNS is", "loopback connections are", "DNS and loopback connections are".
pub fn refused_subject(gaps: &str) -> String {
    match gaps {
        "" => "a filtered path is".to_owned(),
        "DNS" => "DNS is".to_owned(),
        other => format!("{other} are"),
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
    let refused_at_setup = |object: WfpObject| {
        record
            .as_ref()
            .ok()
            .copied()
            .flatten()
            .and_then(|r| r.dns_refused.iter().find(|(o, _)| *o == object))
            .map(|(_, why)| why.clone())
    };
    for (object, key) in WfpKeys::derive(names).filters() {
        let state = match &observed.wfp_unreadable {
            Some(why) => match refused_at_setup(object) {
                // The record says it was never added, whatever a read that
                // was refused would have shown.
                Some(refused) => ItemState::Failed(format!(
                    "Windows refused it at setup: {refused}; {}",
                    gap_words(object)
                )),
                None => ItemState::Unknown(why.clone()),
            },
            // Present is not effective: a filter the Base Filtering Engine
            // has disabled is listed and read like any other.
            None if observed.wfp_disabled.contains(&key) => ItemState::Wrong(
                "present, but the Base Filtering Engine has it disabled, so it does not run"
                    .to_owned(),
            ),
            None if observed.wfp.contains(&key) => ItemState::Present,
            None => match refused_at_setup(object) {
                Some(refused) => ItemState::Failed(format!(
                    "Windows refused it at setup: {refused}; {}",
                    gap_words(object)
                )),
                None => ItemState::Missing,
            },
        };
        // Present is not proved: only `kgraf sandbox selfcheck` shows the
        // effect.
        let item = if object.is_loopback_filter() {
            format!(
                "{LOOPBACK_FILTER_ITEM}{} {} (blocks {}; `kgraf sandbox selfcheck` proves the \
                 effect)",
                object.layer_words(),
                guid_text(key),
                object.loopback_words()
            )
        } else {
            let what = if leaves_local_lookups_open(object) {
                "meant to refuse the offline users' lookups; `kgraf sandbox selfcheck` proves \
                 the effect"
            } else {
                "remote callers of the DNS Client service only; a lookup on this machine does \
                 not pass this layer"
            };
            format!(
                "{DNS_FILTER_ITEM}{} {} ({what})",
                object.layer_words(),
                guid_text(key)
            )
        };
        push(item, state);
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

/// What a refused filter leaves open, for its status line.
fn gap_words(object: WfpObject) -> &'static str {
    if object.is_loopback_filter() {
        "loopback connections are not confined on this machine: a sandbox user without the \
         network can reach a local service outside kGraf's egress proxy ports"
    } else if leaves_local_lookups_open(object) {
        "DNS is not confined on this machine: a name lookup by a sandbox user without the \
         network can leave it"
    } else {
        "remote callers of the DNS Client service are not filtered"
    }
}

#[cfg(test)]
mod tests {
    use super::super::names::MAX_SLOTS;
    use super::*;

    /// A SID lookup for a host test: no SID resolves, which is the state of
    /// a sandbox group an earlier setup made and deleted.
    fn all_gone(_sid: &str) -> Option<bool> {
        Some(false)
    }

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

    #[test]
    fn a_removal_that_left_a_folder_fails_at_the_end_naming_it() {
        assert_eq!(removal_outcome(&[], None), Ok(()));
        let left = [
            r"C:\work\a keeps its entries for the sandbox users: os error 5".to_owned(),
            r"C:\work\b keeps its entries for the sandbox users: os error 21".to_owned(),
        ];
        let err = removal_outcome(&left, None).unwrap_err();
        assert!(err.starts_with("2 place(s) kept"), "{err}");
        assert!(
            err.contains(r"C:\work\a keeps") && err.contains(r"C:\work\b keeps"),
            "{err}"
        );
        // A failed elevated step is named first, and the folders still are.
        let err = removal_outcome(&left, Some("step remove-user:x failed")).unwrap_err();
        assert!(
            err.starts_with("step remove-user:x failed; 2 place(s)"),
            "{err}"
        );
        assert_eq!(
            removal_outcome(&[], Some("step remove-user:x failed")),
            Err("step remove-user:x failed".to_owned())
        );
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
        // both rights sets, every rule and every WFP object, and nothing
        // else.
        let r = record_from(&steps, &n);
        assert_eq!(
            r.entries.len(),
            2 + 1 + 2 + 2 + RulePurpose::ALL.len() + WfpObject::ALL.len()
        );
        for (object, key) in WfpKeys::derive(&n).all() {
            assert!(
                r.entries.contains(&UndoEntry::Wfp { object, key }),
                "{object}"
            );
        }
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
        let steps = plan_remove(&n, &r, &o, &all_gone).unwrap();
        assert_eq!(steps.len(), r.entries.len());
        assert!(steps.iter().all(|s| s.action == RemoveAction::Remove));
        let rank = |e: &UndoEntry| match e {
            UndoEntry::Wfp { object, .. } => match object {
                o if o.is_filter() => 5,
                WfpObject::Sublayer => 6,
                WfpObject::Provider => 7,
                o => panic!("{o:?} has no rank"),
            },
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
        // The WFP objects are last of all, so a filter that will not go
        // cannot stop the removal of an account, and each filter goes before
        // the sublayer and the provider it names.
        let is_wfp = |s: &RemoveStep| matches!(s.entry, UndoEntry::Wfp { .. });
        let first_wfp = steps.iter().position(is_wfp).expect("WFP steps");
        assert!(
            steps[first_wfp..].iter().all(is_wfp),
            "an account step runs after a WFP step"
        );
        let object_of = |s: &RemoveStep| match s.entry {
            UndoEntry::Wfp { object, .. } => object,
            _ => panic!("not a WFP step"),
        };
        assert_eq!(object_of(&steps[steps.len() - 1]), WfpObject::Provider);
        assert_eq!(object_of(&steps[steps.len() - 2]), WfpObject::Sublayer);
        for object in WfpObject::FILTERS {
            let at = steps
                .iter()
                .position(|s| matches!(s.entry, UndoEntry::Wfp { object: o, .. } if o == object))
                .unwrap_or_else(|| panic!("{object:?} has no step"));
            assert!(
                at < steps.len() - 2,
                "{object:?} is not before the sublayer"
            );
        }
        // Only listed things: a record with one rule removes one rule.
        let mut one = Record::new(OWNER, None);
        one.add(UndoEntry::Rule(n.rule(RulePurpose::BlockOutbound)));
        let steps = plan_remove(&n, &one, &o, &all_gone).unwrap();
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn remove_twice_finds_everything_gone() {
        let n = names();
        let r = record_from(&plan_setup(&n, &full(&n)).unwrap(), &n);
        let empty = Observed::default();
        let steps = plan_remove(&n, &r, &empty, &all_gone).unwrap();
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
                matches!(
                    plan_remove(&n, &r, &o, &all_gone),
                    Err(PlanError::ForeignEntry(_))
                ),
                "{foreign:?}"
            );
        }
        let other = Record::new("S-1-5-21-9-9-9-1001", None);
        assert!(matches!(
            plan_remove(&n, &other, &o, &all_gone),
            Err(PlanError::RecordOwner { .. })
        ));
        let tagged = Record::new(OWNER, Some("wintest"));
        assert!(matches!(
            plan_remove(&n, &tagged, &o, &all_gone),
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
            plan_remove(&n, &r, &o, &all_gone),
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
        // Remove deletes every pool user the record lists. A removal does not
        // know the slot count, so it reads every slot user there can be.
        let steps = plan_remove(&names().with_slots(MAX_SLOTS), &r, &o, &all_gone).unwrap();
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

    /// A removal whose names cover fewer slots than the record lists read
    /// none of the other users, so each looks absent. It must refuse, not
    /// plan them as already gone: that drops their record lines, and the
    /// users, their sign-in screen entries and their rights stay on the
    /// machine with nothing left that lists them.
    #[test]
    fn a_removal_that_did_not_read_a_slot_user_refuses_instead_of_dropping_it() {
        let n = names().with_slots(3);
        let full = full_with_slots(&n);
        let r = record_from(&plan_setup(&n, &full).unwrap(), &n);
        // What the machine looks like through names that cover no slots.
        let fixed = names();
        let mut seen = full.clone();
        for role in n.roles() {
            if !fixed.roles().contains(&role) {
                let user = n.user(role);
                seen.users.remove(&user);
                seen.hidden.remove(&user);
                seen.rights.remove(&user);
            }
        }
        let err = plan_remove(&fixed, &r, &seen, &all_gone).unwrap_err();
        assert!(matches!(err, PlanError::NotRead(_)), "{err}");
        // Names that cover every slot read them all, and remove them all.
        let all = names().with_slots(MAX_SLOTS);
        let steps = plan_remove(&all, &r, &full, &all_gone).unwrap();
        for role in n.roles() {
            let user = n.user(role);
            for entry in [
                UndoEntry::User(user.clone()),
                UndoEntry::Hidden(user.clone()),
            ] {
                assert!(
                    steps
                        .iter()
                        .any(|s| s.entry == entry && s.action == RemoveAction::Remove),
                    "{entry:?} is not removed"
                );
            }
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
        // again. One missing filter of the three is enough: the layer it
        // covers is open until the step runs.
        for (_, key) in WfpKeys::derive(&n).filters() {
            let mut half = full(&n);
            half.wfp.remove(&key);
            let step = plan_setup(&n, &half)
                .unwrap()
                .into_iter()
                .find(|s| s.id == StepId::DnsFilter)
                .unwrap();
            assert_eq!(step.action, Action::Create, "{}", guid_text(key));
        }
    }

    #[test]
    fn remove_deletes_the_dns_filter_before_its_sublayer_and_provider() {
        let n = names();
        let o = full(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        let steps = plan_remove(&n, &r, &o, &all_gone).unwrap();
        let wfp: Vec<(WfpObject, RemoveAction)> = steps
            .iter()
            .filter_map(|s| match s.entry {
                UndoEntry::Wfp { object, .. } => Some((object, s.action)),
                _ => None,
            })
            .collect();
        let mut expected: Vec<(WfpObject, RemoveAction)> = WfpObject::FILTERS
            .into_iter()
            .map(|o| (o, RemoveAction::Remove))
            .collect();
        expected.push((WfpObject::Sublayer, RemoveAction::Remove));
        expected.push((WfpObject::Provider, RemoveAction::Remove));
        assert_eq!(wfp, expected, "every filter before the sublayer");
        // The WFP objects go last, after every account: a block filter that
        // will not go must never stop the removal of a user.
        let first_wfp = steps
            .iter()
            .position(|s| matches!(s.entry, UndoEntry::Wfp { .. }))
            .expect("a WFP step");
        assert_eq!(
            first_wfp,
            steps.len() - WfpObject::ALL.len(),
            "the WFP steps are the last ones: {:?}",
            steps.iter().map(RemoveStep::key).collect::<Vec<String>>()
        );
        for kind in [
            UndoEntry::User(String::new()),
            UndoEntry::Group(String::new()),
            UndoEntry::Rule(String::new()),
        ] {
            let at = steps
                .iter()
                .position(|s| std::mem::discriminant(&s.entry) == std::mem::discriminant(&kind))
                .unwrap_or_else(|| panic!("a step of {kind:?}"));
            assert!(at < first_wfp, "{kind:?} is removed before the filters");
        }
        let keys: Vec<String> = steps[first_wfp..].iter().map(RemoveStep::key).collect();
        assert!(
            keys.contains(&format!(
                "remove-wfp-filter:{}",
                guid_text(WfpKeys::derive(&n).rpc_filter)
            )),
            "{keys:?}"
        );
        // Gone already: nothing to do.
        let gone = plan_remove(&n, &r, &Observed::default(), &all_gone).unwrap();
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
            plan_remove(&n, &r, &unknown, &all_gone)
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
                object: WfpObject::RpcFilter,
                key: WfpKeys::derive(&n).provider,
            },
            UndoEntry::Wfp {
                object: WfpObject::RpcFilter,
                key: 0x75a89dda_95e4_40f3_adc7_7688a9c847e1,
            },
            // One filter's key under another filter's name is foreign too.
            UndoEntry::Wfp {
                object: WfpObject::NameCacheFilterV4,
                key: WfpKeys::derive(&n).name_cache_v6,
            },
        ] {
            let mut r = Record::new(OWNER, None);
            r.add(foreign.clone());
            assert!(
                matches!(
                    plan_remove(&n, &r, &o, &all_gone),
                    Err(PlanError::ForeignEntry(_))
                ),
                "{foreign:?}"
            );
        }
    }

    #[test]
    fn status_reports_every_wfp_filter() {
        let n = names();
        let lines = |o: &Observed| -> Vec<StatusLine> {
            status_report(&n, o, Ok(None), &Credentials::Missing)
                .lines
                .into_iter()
                .filter(|l| {
                    l.item.starts_with(DNS_FILTER_ITEM) || l.item.starts_with(LOOPBACK_FILTER_ITEM)
                })
                .collect()
        };
        let o = full(&n);
        // One line per filter, each naming its layer and its own key: a
        // single line could report present while another layer is open.
        let got = lines(&o);
        assert_eq!(got.len(), WfpObject::FILTERS.len(), "{got:?}");
        for (object, key) in WfpKeys::derive(&n).filters() {
            let line = got
                .iter()
                .find(|l| l.item.contains(&guid_text(key)))
                .unwrap_or_else(|| panic!("no line for {object}: {got:?}"));
            assert_eq!(line.state, ItemState::Present);
            assert!(line.item.contains(object.layer_words()), "{line:?}");
            // A loopback filter's line says so and what it blocks; a DNS
            // filter's line says DNS.
            if object.is_loopback_filter() {
                assert!(line.item.starts_with(LOOPBACK_FILTER_ITEM), "{line:?}");
                assert!(line.item.contains(object.loopback_words()), "{line:?}");
            } else {
                assert!(line.item.starts_with(DNS_FILTER_ITEM), "{line:?}");
            }
        }
        // One missing filter is reported, and makes the setup not ready.
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        for (_, key) in WfpKeys::derive(&n).filters() {
            let mut missing = o.clone();
            missing.wfp.remove(&key);
            let line = lines(&missing)
                .into_iter()
                .find(|l| l.item.contains(&guid_text(key)))
                .expect("a line for the missing filter");
            assert_eq!(line.state, ItemState::Missing);
            assert!(!status_report(&n, &missing, Ok(Some(&r)), &all_stored(&n)).ready);
        }
        let mut unread = o.clone();
        unread.wfp.clear();
        unread.wfp_unreadable = Some("needs administrator rights".into());
        assert!(
            lines(&unread)
                .iter()
                .all(|l| matches!(l.state, ItemState::Unknown(_)))
        );
    }

    /// The refusal Windows Server 2025 gave the name resolution cache filter.
    const OUT_OF_BOUNDS: &str = "WFP error 0x80320028 (FWP_E_OUT_OF_BOUNDS)";

    /// A filter Windows refuses does not fail the DNS step: the step is
    /// `partial`, each refused filter is listed with why, and only a step
    /// that added every filter reads as created or updated.
    #[test]
    fn a_refused_dns_filter_makes_the_step_partial_not_failed() {
        let added = |o: WfpObject| (o, FilterOutcome::Added);
        let refused = |o: WfpObject| (o, FilterOutcome::NotAdded(OUT_OF_BOUNDS.into()));
        // Every filter added: as planned.
        let all: Vec<_> = WfpObject::FILTERS.into_iter().map(added).collect();
        let r = dns_step_report(Action::Create, &all);
        assert_eq!(r.outcome, StepOutcome::Created);
        assert!(r.refused.is_empty());
        assert_eq!(dns_notice(&r.refused), None);
        assert_eq!(
            dns_step_report(Action::Update, &all).outcome,
            StepOutcome::Updated
        );
        // The run on Windows Server 2025: the RPC filter taken, the IPv4
        // name resolution cache filter refused, and so on; the loopback
        // filters taken.
        let server_2025 = vec![
            added(WfpObject::RpcFilter),
            refused(WfpObject::NameCacheFilterV4),
            refused(WfpObject::NameCacheFilterV6),
            added(WfpObject::LoopbackFilterV4),
            added(WfpObject::LoopbackFilterV6),
            added(WfpObject::LoopbackUdpFilterV4),
            added(WfpObject::LoopbackUdpFilterV6),
        ];
        let r = dns_step_report(Action::Create, &server_2025);
        assert_eq!(r.outcome, StepOutcome::Partial);
        assert_eq!(
            r.refused,
            vec![
                (WfpObject::NameCacheFilterV4, OUT_OF_BOUNDS.to_owned()),
                (WfpObject::NameCacheFilterV6, OUT_OF_BOUNDS.to_owned()),
            ]
        );
        let notice = dns_notice(&r.refused).expect("a notice");
        assert!(
            notice.starts_with("DNS is not confined on this machine."),
            "{notice}"
        );
        assert!(!notice.contains("Loopback"), "{notice}");
        for needle in [
            "the name resolution cache layer, IPv4",
            "FWP_E_OUT_OF_BOUNDS",
            "can leave the machine",
            "firewall rules",
            "FAILED",
            "`dns:` line that is not a pass",
        ] {
            assert!(notice.contains(needle), "{needle:?}: {notice}");
        }
        // Only the RPC filter refused: lookups on this machine are not
        // claimed open or closed; selfcheck judges them.
        let rpc_only = dns_step_report(
            Action::Create,
            &[
                refused(WfpObject::RpcFilter),
                added(WfpObject::NameCacheFilterV4),
                added(WfpObject::NameCacheFilterV6),
                added(WfpObject::LoopbackFilterV4),
                added(WfpObject::LoopbackFilterV6),
                added(WfpObject::LoopbackUdpFilterV4),
                added(WfpObject::LoopbackUdpFilterV6),
            ],
        );
        assert_eq!(rpc_only.outcome, StepOutcome::Partial);
        let notice = dns_notice(&rpc_only.refused).unwrap();
        assert!(!notice.contains("not confined"), "{notice}");
        assert!(notice.contains("remote callers"), "{notice}");
        // A filter the result list lost counts as not added, never as added.
        let lost = dns_step_report(Action::Create, &all[..all.len() - 1]);
        assert_eq!(lost.outcome, StepOutcome::Partial);
        assert_eq!(lost.refused.len(), 1);
        assert_eq!(lost.refused[0].0, WfpObject::LoopbackUdpFilterV6);
    }

    /// A loopback filter Windows refuses is a gap of its own: the step is
    /// `partial`, the notice says loopback connections are not confined and
    /// that selfcheck fails, and a DNS refusal beside it keeps its own
    /// paragraph.
    #[test]
    fn a_refused_loopback_filter_is_a_gap_the_notice_names() {
        let added = |o: WfpObject| (o, FilterOutcome::Added);
        let refused = |o: WfpObject| (o, FilterOutcome::NotAdded(OUT_OF_BOUNDS.into()));
        let results: Vec<_> = WfpObject::FILTERS
            .into_iter()
            .map(|o| {
                if o == WfpObject::LoopbackFilterV4 {
                    refused(o)
                } else {
                    added(o)
                }
            })
            .collect();
        let r = dns_step_report(Action::Create, &results);
        assert_eq!(r.outcome, StepOutcome::Partial);
        assert_eq!(
            r.refused,
            vec![(WfpObject::LoopbackFilterV4, OUT_OF_BOUNDS.to_owned())]
        );
        let notice = dns_notice(&r.refused).expect("a notice");
        assert!(
            notice.starts_with("Loopback connections are not confined on this machine."),
            "{notice}"
        );
        for needle in [
            "the connect layer, IPv4",
            "egress proxy ports",
            "FWP_E_OUT_OF_BOUNDS",
            "FAILED",
            "selfcheck` fails",
        ] {
            assert!(notice.contains(needle), "{needle:?}: {notice}");
        }
        assert!(!notice.contains("DNS"), "{notice}");
        // Both kinds refused: one paragraph each, loopback first.
        let both = dns_notice(&[
            (WfpObject::NameCacheFilterV4, OUT_OF_BOUNDS.into()),
            (WfpObject::LoopbackUdpFilterV6, OUT_OF_BOUNDS.into()),
        ])
        .unwrap();
        let paragraphs: Vec<&str> = both.lines().collect();
        assert_eq!(paragraphs.len(), 2, "{both}");
        assert!(paragraphs[0].starts_with("Loopback"), "{both}");
        assert!(paragraphs[1].starts_with("DNS is not confined"), "{both}");
    }

    /// A loopback filter the last setup recorded as refused is a FAILED line
    /// that says loopback connections are not confined, and the closing
    /// sentence and the summary say the same, not DNS.
    #[test]
    fn status_shows_a_refused_loopback_filter_as_failed() {
        let n = names();
        let mut o = full(&n);
        let keys = WfpKeys::derive(&n);
        o.wfp.remove(&keys.loopback_v6);
        let mut r = record_from(&plan_setup(&n, &full(&n)).unwrap(), &n);
        r.dns_refused = vec![(WfpObject::LoopbackFilterV6, OUT_OF_BOUNDS.into())];
        let report = status_report(&n, &o, Ok(Some(&r)), &all_stored(&n));
        let line = report
            .lines
            .iter()
            .find(|l| l.item.contains(&guid_text(keys.loopback_v6)))
            .unwrap();
        assert!(line.item.starts_with(LOOPBACK_FILTER_ITEM), "{line:?}");
        let ItemState::Failed(why) = &line.state else {
            panic!("{line:?}");
        };
        assert!(why.contains(OUT_OF_BOUNDS), "{why}");
        assert!(
            why.contains("loopback connections are not confined"),
            "{why}"
        );
        assert!(report.only_refused_gaps());
        assert_eq!(report.refused_gaps(), "loopback connections");
        assert_eq!(
            refused_subject(&report.refused_gaps()),
            "loopback connections are"
        );
        let text = report.to_string();
        assert!(
            text.contains("but loopback connections are not confined on this machine"),
            "{text}"
        );
        // Both kinds refused.
        r.dns_refused
            .push((WfpObject::NameCacheFilterV4, OUT_OF_BOUNDS.into()));
        o.wfp.remove(&keys.name_cache_v4);
        let report = status_report(&n, &o, Ok(Some(&r)), &all_stored(&n));
        assert_eq!(report.refused_gaps(), "DNS and loopback connections");
        assert_eq!(refused_subject("DNS"), "DNS is");
    }

    /// A filter the last setup recorded as refused is a FAILED status line
    /// with the reason, not a MISSING one that `kgraf sandbox setup` would
    /// fix; the setup is not ready, and the closing sentence says DNS is not
    /// confined instead of asking for setup again.
    #[test]
    fn status_shows_a_refused_dns_filter_as_failed() {
        let n = names();
        let mut o = full(&n);
        let keys = WfpKeys::derive(&n);
        o.wfp.remove(&keys.name_cache_v4);
        o.wfp.remove(&keys.name_cache_v6);
        let mut r = record_from(&plan_setup(&n, &full(&n)).unwrap(), &n);
        r.dns_refused = vec![
            (WfpObject::NameCacheFilterV4, OUT_OF_BOUNDS.into()),
            (WfpObject::NameCacheFilterV6, OUT_OF_BOUNDS.into()),
        ];
        let report = status_report(&n, &o, Ok(Some(&r)), &all_stored(&n));
        let dns: Vec<&StatusLine> = report
            .lines
            .iter()
            .filter(|l| l.item.contains("DNS filter"))
            .collect();
        for object in [WfpObject::NameCacheFilterV4, WfpObject::NameCacheFilterV6] {
            let line = dns
                .iter()
                .find(|l| l.item.contains(object.layer_words()))
                .unwrap();
            let ItemState::Failed(why) = &line.state else {
                panic!("{line:?}");
            };
            assert!(why.contains(OUT_OF_BOUNDS), "{why}");
            assert!(why.contains("DNS is not confined"), "{why}");
        }
        let rpc = dns
            .iter()
            .find(|l| l.item.contains(WfpObject::RpcFilter.layer_words()))
            .unwrap();
        assert_eq!(rpc.state, ItemState::Present);
        assert!(rpc.item.contains("remote callers"), "{}", rpc.item);
        assert!(!report.ready);
        assert!(report.only_refused_gaps());
        let text = report.to_string();
        assert!(text.contains("  FAILED   DNS filter on"), "{text}");
        assert!(
            text.ends_with("running setup again does not change that."),
            "{text}"
        );
        assert!(!text.contains("run `kgraf sandbox setup`"), "{text}");
        // A missing filter the record does not name stays MISSING, and asks
        // for setup.
        let mut plain = r.clone();
        plain.dns_refused.clear();
        let report = status_report(&n, &o, Ok(Some(&plain)), &all_stored(&n));
        assert!(!report.only_refused_gaps());
        assert!(report.to_string().contains("run `kgraf sandbox setup`"));
        // A refused filter with another gap beside it: setup is still asked
        // for, and the DNS line still says FAILED.
        let mut worse = o.clone();
        worse.rules.clear();
        let report = status_report(&n, &worse, Ok(Some(&r)), &all_stored(&n));
        assert!(!report.only_refused_gaps());
        assert!(
            report
                .lines
                .iter()
                .any(|l| matches!(l.state, ItemState::Failed(_)))
        );
        // Unreadable WFP state: the record still says what setup could not
        // add.
        let mut unread = o.clone();
        unread.wfp.clear();
        unread.wfp_unreadable = Some("needs administrator rights".into());
        let report = status_report(&n, &unread, Ok(Some(&r)), &all_stored(&n));
        let failed = report
            .lines
            .iter()
            .filter(|l| matches!(l.state, ItemState::Failed(_)))
            .count();
        assert_eq!(failed, 2);
    }

    /// The record lists every WFP key whatever Windows added, and a removal
    /// takes off the ones that exist and passes over the ones Windows never
    /// added, so a partial set is removed in full.
    #[test]
    fn remove_takes_off_a_partial_dns_filter_set() {
        let n = names();
        let keys = WfpKeys::derive(&n);
        let mut o = full(&n);
        o.wfp.remove(&keys.name_cache_v4);
        o.wfp.remove(&keys.name_cache_v6);
        let r = record_from(&plan_setup(&n, &full(&n)).unwrap(), &n);
        let wfp: Vec<(WfpObject, RemoveAction)> = plan_remove(&n, &r, &o, &all_gone)
            .unwrap()
            .into_iter()
            .filter_map(|s| match s.entry {
                UndoEntry::Wfp { object, .. } => Some((object, s.action)),
                _ => None,
            })
            .collect();
        assert_eq!(
            wfp,
            vec![
                (WfpObject::RpcFilter, RemoveAction::Remove),
                (WfpObject::NameCacheFilterV4, RemoveAction::AlreadyGone),
                (WfpObject::NameCacheFilterV6, RemoveAction::AlreadyGone),
                (WfpObject::LoopbackFilterV4, RemoveAction::Remove),
                (WfpObject::LoopbackFilterV6, RemoveAction::Remove),
                (WfpObject::LoopbackUdpFilterV4, RemoveAction::Remove),
                (WfpObject::LoopbackUdpFilterV6, RemoveAction::Remove),
                (WfpObject::Sublayer, RemoveAction::Remove),
                (WfpObject::Provider, RemoveAction::Remove),
            ]
        );
    }

    /// A setup that fails at the DNS step must say what it leaves: the
    /// credentials of an earlier setup stay, so commands keep running as a
    /// sandbox user with no filter.
    #[test]
    fn a_failed_dns_step_says_what_it_leaves_behind() {
        let left = failure_leaves(StepId::DnsFilter);
        assert!(!left.is_empty());
        for needle in [
            "stored passwords of an earlier setup stay",
            "a name lookup can leave the machine",
            "kgraf sandbox status",
            "kgraf sandbox selfcheck",
            "kgraf sandbox remove",
        ] {
            assert!(left.contains(needle), "{needle:?} is missing: {left}");
        }
        // Every other step is covered by its own error; no clause.
        let n = names();
        for step in plan_setup(
            &n,
            &Observed {
                firewall: firewall_ok(),
                ..Observed::default()
            },
        )
        .unwrap()
        {
            if step.id != StepId::DnsFilter {
                assert_eq!(failure_leaves(step.id), "", "{:?}", step.id);
            }
        }
    }

    /// A filter the Base Filtering Engine has disabled exists and does not
    /// run. Status must not call it present, and the setup must not call
    /// itself ready or the step an Update.
    #[test]
    fn a_disabled_filter_is_present_but_not_effective() {
        let n = names();
        let o = full(&n);
        let r = record_from(&plan_setup(&n, &o).unwrap(), &n);
        for (object, key) in WfpKeys::derive(&n).filters() {
            let mut off = o.clone();
            off.wfp_disabled.insert(key);
            let report = status_report(&n, &off, Ok(Some(&r)), &all_stored(&n));
            let line = report
                .lines
                .iter()
                .find(|l| l.item.contains(&guid_text(key)))
                .unwrap_or_else(|| panic!("a line for {object}"));
            let ItemState::Wrong(why) = &line.state else {
                panic!("{object} is disabled, not {:?}", line.state);
            };
            assert!(why.contains("disabled"), "{why}");
            assert!(!report.ready, "{object} disabled is not ready");
            // The step replaces the filters, so it is a Create, not an
            // Update of something that already works.
            let step = plan_setup(&n, &off)
                .unwrap()
                .into_iter()
                .find(|s| s.id == StepId::DnsFilter)
                .expect("the DNS filter step");
            assert_eq!(step.action, Action::Create, "{object}");
        }
    }
}

#[cfg(test)]
mod closed_folder_tests {
    use super::*;

    /// A SID lookup for a host test: no SID resolves, which is the state of
    /// a sandbox group an earlier setup made and deleted.
    fn all_gone(_sid: &str) -> Option<bool> {
        Some(false)
    }

    const OWNER: &str = "S-1-5-21-1-2-3-1001";
    const GROUP_SID: &str = "S-1-5-21-1-2-3-1100";

    fn names() -> OwnerNames {
        OwnerNames::derive(OWNER, None).unwrap()
    }

    /// The folders this machine would close. Removal takes an entry off only
    /// on one of these.
    fn closable() -> BTreeSet<String> {
        [
            r"C:\",
            r"C:\ProgramData",
            r"C:\Users\Public",
            r"C:\Windows\Temp",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// The plan keeps the one `closed` entry of `r` and undoes nothing of
    /// it, with a reason. A refusal of the whole plan would be the defect a
    /// deleted group once caused.
    fn is_kept(
        n: &OwnerNames,
        r: &Record,
        o: &Observed,
        resolves: &dyn Fn(&str) -> Option<bool>,
    ) -> bool {
        let steps = plan_remove(n, r, o, resolves).expect("a plan, never a refusal");
        steps
            .iter()
            .filter(|s| matches!(s.entry, UndoEntry::Closed { .. }))
            .all(|s| s.action == RemoveAction::Kept && s.note.is_some())
    }

    fn closed(sid: &str, path: &str) -> UndoEntry {
        UndoEntry::Closed {
            sid: sid.into(),
            path: path.into(),
            mask: super::super::acl::CLOSED_FOLDER_DENY,
            flags: super::super::acl::CLOSED_FOLDER_FLAGS,
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
            closable: closable(),
            ..Observed::default()
        };
        let mut r = Record::new(OWNER, None);
        r.add(UndoEntry::Group(n.group()));
        r.add(closed(GROUP_SID, r"C:\ProgramData"));
        let steps = plan_remove(&n, &r, &o, &all_gone).unwrap();
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
            closable: closable(),
            ..Observed::default()
        };
        // Users, Everyone, SYSTEM, the owner's own account, and a domain
        // group, whose SID starts with S-1-5-21- as every local one does.
        for sid in [
            "S-1-5-32-545",
            "S-1-1-0",
            "S-1-5-18",
            "S-1-5-21-1-2-3-1001",
            "S-1-5-21-9-9-9-513",
            "S-1-5-21-9-9-9-1101",
        ] {
            let mut r = Record::new(OWNER, None);
            r.add(closed(sid, r"C:\"));
            assert!(is_kept(&n, &r, &o, &all_gone), "{sid}");
        }
        // With the group gone, the SID of a group an earlier setup made is
        // taken: it is of the owner's authority and no longer resolves.
        let gone = Observed {
            closable: closable(),
            ..Observed::default()
        };
        let mut r = Record::new(OWNER, None);
        r.add(closed(GROUP_SID, r"C:\"));
        assert!(plan_remove(&n, &r, &gone, &all_gone).is_ok());
        // A SID that still resolves names a live principal: not taken, and
        // neither is one whose lookup failed.
        assert!(is_kept(&n, &r, &gone, &|_| Some(true)));
        assert!(is_kept(&n, &r, &gone, &|_| None));
    }

    /// A deny entry whose group was deleted outside kGraf must never block
    /// the whole removal: setup cleans the stale generation up, and a
    /// removal that still meets one keeps that entry alone and undoes the
    /// rest.
    #[test]
    fn a_stale_group_generation_is_cleaned_up_and_blocks_nothing() {
        let n = names();
        const OLD_GROUP: &str = "S-1-5-21-1-2-3-1050";
        // The group of the first setup is gone and the second setup made a
        // new one. Both generations are in the record.
        let o = Observed {
            group: Some(n.group_comment()),
            group_sid: Some(GROUP_SID.into()),
            closable: closable(),
            ..Observed::default()
        };
        let mut r = Record::new(OWNER, None);
        r.add(UndoEntry::Group(n.group()));
        r.add(closed(OLD_GROUP, r"C:\ProgramData"));
        r.add(closed(OLD_GROUP, r"C:\"));
        r.add(closed(GROUP_SID, r"C:\ProgramData"));
        // Setup takes the old generation off and leaves its own alone.
        let stale = stale_closed_entries(&n, &r, &o, &all_gone);
        assert_eq!(
            stale,
            vec![
                closed(OLD_GROUP, r"C:\ProgramData"),
                closed(OLD_GROUP, r"C:\")
            ]
        );
        // A group that still exists is another owner's: left where it is.
        assert!(stale_closed_entries(&n, &r, &o, &|_| Some(true)).is_empty());
        // Without the group of the current setup there is nothing to compare
        // against, so setup takes nothing off.
        let no_group = Observed {
            closable: closable(),
            ..Observed::default()
        };
        assert!(stale_closed_entries(&n, &r, &no_group, &all_gone).is_empty());
        // Removal undoes everything it may, and keeps what it may not: here
        // a domain group's entry, which no removal of kGraf's may touch.
        let mut edited = Record::new(OWNER, None);
        edited.add(UndoEntry::Group(n.group()));
        edited.add(closed("S-1-5-21-9-9-9-513", r"C:\ProgramData"));
        edited.add(closed(GROUP_SID, r"C:\"));
        let steps = plan_remove(&n, &edited, &o, &all_gone).unwrap();
        assert_eq!(steps.len(), 3, "{steps:?}");
        let by = |e: &UndoEntry| {
            steps
                .iter()
                .find(|s| s.entry == *e)
                .cloned()
                .expect("a step per entry")
        };
        let kept = by(&closed("S-1-5-21-9-9-9-513", r"C:\ProgramData"));
        assert_eq!(kept.action, RemoveAction::Kept);
        assert!(kept.note.is_some_and(|w| w.contains("S-1-5-21-9-9-9-513")));
        assert_eq!(by(&closed(GROUP_SID, r"C:\")).action, RemoveAction::Remove);
        assert_eq!(
            by(&UndoEntry::Group(n.group())).action,
            RemoveAction::Remove,
            "the group still goes"
        );
    }

    /// A path the record names must be one of the folders setup closes on
    /// this machine, so an edited line cannot send the removal at the access
    /// list of another folder.
    #[test]
    fn remove_refuses_a_deny_entry_on_a_folder_setup_never_closes() {
        let n = names();
        let o = Observed {
            group: Some(n.group_comment()),
            group_sid: Some(GROUP_SID.into()),
            closable: closable(),
            ..Observed::default()
        };
        for path in [
            r"C:\Users\owner\Documents",
            r"C:\Windows\System32",
            r"D:\",
            r"C:",
            "",
        ] {
            let mut r = Record::new(OWNER, None);
            r.add(closed(GROUP_SID, path));
            assert!(is_kept(&n, &r, &o, &all_gone), "{path:?}");
        }
        // The same folder written in another case is the same folder.
        let mut r = Record::new(OWNER, None);
        r.add(closed(GROUP_SID, r"c:\programdata"));
        assert!(plan_remove(&n, &r, &o, &all_gone).is_ok());
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
