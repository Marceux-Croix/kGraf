//! Names of everything the setup creates, derived from the owner's SID.
//!
//! A Windows local user name holds at most 20 characters. The owner's own
//! name can be longer, can hold characters a user name refuses, and can be
//! renamed; the SID is fixed. So each name is a fixed prefix plus 8 hex digits
//! of an FNV-1a hash of the owner's SID (and an optional tag, which the
//! Windows tests use so they never touch a real setup). A hash collision with
//! another owner on the same machine is caught by the comment marker each
//! user carries, which names the owner's SID in full.

use std::fmt;
use std::path::Path;

use super::firewall::RulePurpose;

/// The most characters a Windows local user name may hold.
pub const MAX_USER_NAME: usize = 20;

/// The most characters of a tag.
pub const MAX_TAG: usize = 12;

/// The most worker slots a setup may create users for. A slot user's name is
/// `kgraf-sNN-<8 hex>`, 18 characters, so two digits are the most that fit in
/// [`MAX_USER_NAME`].
pub const MAX_SLOTS: u8 = 99;

/// How many slot users `kgraf sandbox setup` creates when none is asked for.
/// Commands that run at the same time in different worktrees then run as
/// different users, so neither can write the other's files.
pub const DEFAULT_SLOTS: u8 = 4;

/// Which sandbox user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Commands without the network, or limited to kGraf's egress proxy,
    /// where the setup created no slot users. Firewall rules block this
    /// user's traffic.
    Offline,
    /// Commands with the network open. No firewall rules.
    Online,
    /// One worker slot, confined like [`Role::Offline`] and blocked by the
    /// same firewall rules. Commands in different worktrees take different
    /// slots, so one cannot write another's files.
    Slot(u8),
}

impl Role {
    /// The two roles every setup has. Slot roles come from the setup's own
    /// count ([`OwnerNames::roles`]).
    pub const FIXED: [Role; 2] = [Role::Offline, Role::Online];

    /// The word in messages and records: `offline`, `online`, `slot07`.
    pub fn key(self) -> String {
        match self {
            Role::Offline => "offline".to_owned(),
            Role::Online => "online".to_owned(),
            Role::Slot(i) => format!("slot{i:02}"),
        }
    }

    pub fn from_key(s: &str) -> Option<Role> {
        match s {
            "offline" => Some(Role::Offline),
            "online" => Some(Role::Online),
            _ => {
                let digits = s.strip_prefix("slot")?;
                // Exactly two digits, so one slot has one name.
                if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let i: u8 = digits.parse().ok()?;
                (i < MAX_SLOTS).then_some(Role::Slot(i))
            }
        }
    }

    /// Whether the firewall rules block this user's network.
    pub fn blocked(self) -> bool {
        matches!(self, Role::Offline | Role::Slot(_))
    }

    fn short(self) -> String {
        match self {
            Role::Offline => "off".to_owned(),
            Role::Online => "on".to_owned(),
            Role::Slot(i) => format!("s{i:02}"),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("{0:?} is not a SID in string form (S-1-...)")]
    BadSid(String),
    #[error("the tag {0:?} must be 1 to {MAX_TAG} lowercase ASCII letters or digits")]
    BadTag(String),
}

/// The names for one owner (and tag).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerNames {
    owner_sid: String,
    tag: Option<String>,
    hash: String,
    /// How many slot users these names cover.
    slots: u8,
}

impl OwnerNames {
    pub fn derive(owner_sid: &str, tag: Option<&str>) -> Result<Self, NameError> {
        if !is_sid_string(owner_sid) {
            return Err(NameError::BadSid(owner_sid.to_owned()));
        }
        if let Some(t) = tag
            && !is_tag(t)
        {
            return Err(NameError::BadTag(t.to_owned()));
        }
        let mut input = owner_sid.to_owned();
        if let Some(t) = tag {
            input.push('/');
            input.push_str(t);
        }
        // 32 bits are enough to keep two owners of one machine apart; a
        // collision is caught by the comment marker, never acted on.
        let hash = format!("{:08x}", fnv1a64(input.as_bytes()) as u32);
        Ok(Self {
            owner_sid: owner_sid.to_owned(),
            tag: tag.map(str::to_owned),
            hash,
            slots: 0,
        })
    }

    /// The same names covering `slots` worker slots. More than
    /// [`MAX_SLOTS`] is cut to that.
    #[must_use]
    pub fn with_slots(mut self, slots: u8) -> Self {
        self.slots = slots.min(MAX_SLOTS);
        self
    }

    pub fn slots(&self) -> u8 {
        self.slots
    }

    /// Every role these names cover: the two fixed ones and one per slot.
    pub fn roles(&self) -> Vec<Role> {
        Role::FIXED
            .into_iter()
            .chain((0..self.slots).map(Role::Slot))
            .collect()
    }

    pub fn owner_sid(&self) -> &str {
        &self.owner_sid
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// `kgraf-off-1a2b3c4d`, `kgraf-on-1a2b3c4d`, `kgraf-s00-1a2b3c4d`.
    pub fn user(&self, role: Role) -> String {
        format!("kgraf-{}-{}", role.short(), self.hash)
    }

    /// Every user name these names cover, in role order.
    pub fn users(&self) -> Vec<String> {
        self.roles().iter().map(|r| self.user(*r)).collect()
    }

    /// The role of the user `user`, whether or not these names cover its
    /// slot: a removal must recognize a user an earlier, larger setup made.
    pub fn role_of(&self, user: &str) -> Option<Role> {
        Role::FIXED
            .into_iter()
            .chain((0..MAX_SLOTS).map(Role::Slot))
            .find(|r| self.user(*r) == user)
    }

    /// The local group both users belong to, for the ACLs of later units.
    pub fn group(&self) -> String {
        format!("kgraf-sandbox-{}", self.hash)
    }

    pub fn rule(&self, purpose: RulePurpose) -> String {
        format!("kgraf-sandbox-{}-{}", self.hash, purpose.key())
    }

    pub fn rules(&self) -> Vec<String> {
        RulePurpose::ALL.iter().map(|p| self.rule(*p)).collect()
    }

    /// The comment the setup writes on a user: what it is and whose.
    pub fn user_comment(&self, role: Role) -> String {
        format!("kGraf sandbox user ({}) for {}", role.key(), self.owner_sid)
    }

    pub fn group_comment(&self) -> String {
        format!("kGraf sandbox users for {}", self.owner_sid)
    }

    /// Whether an existing user's comment says kGraf made it for this owner.
    /// A user with the right name and another comment is not ours: the
    /// setup refuses it rather than take it over.
    pub fn owns_user_comment(&self, role: Role, comment: &str) -> bool {
        comment == self.user_comment(role)
    }

    pub fn owns_group_comment(&self, comment: &str) -> bool {
        comment == self.group_comment()
    }

    /// Name parts a password must not hold: Windows' complexity policy
    /// refuses a password that contains a part of the account name of 3 or
    /// more characters, split at `-` and other delimiters.
    /// Each user gets its own password, and Windows judges a password
    /// against the name of the account it belongs to, so this lists the
    /// parts of that one name.
    pub fn password_forbidden_parts(&self, role: Role) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        for part in self.user(role).split(['-', '_', '.', ',', ' ', '#']) {
            if part.len() >= 3 && !parts.iter().any(|p| p == part) {
                parts.push(part.to_ascii_lowercase());
            }
        }
        parts
    }
}

/// The worker slot of the writable root `root`, out of `count` slots: an
/// FNV-1a hash of the root's text, lowercased, so one folder always takes
/// one slot however its path was written. Two commands in different
/// worktrees then usually run as different users and neither can write the
/// other's files; two in one worktree share a user, which they must, because
/// they share the folder.
pub fn slot_of(root: &Path, count: u8) -> u8 {
    assert!(count > 0, "a slot is only chosen where slots exist");
    let text = root.to_string_lossy().to_lowercase();
    u8::try_from(fnv1a64(text.as_bytes()) % u64::from(count)).unwrap_or(0)
}

/// Whether `s` is a SID in string form: `S-1-` and dash-separated decimal
/// numbers.
pub fn is_sid_string(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("S-1-") else {
        return false;
    };
    s.len() <= 184
        && !rest.is_empty()
        && rest.split('-').all(|part| {
            !part.is_empty() && part.len() <= 10 && part.bytes().all(|b| b.is_ascii_digit())
        })
}

fn is_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_TAG
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// FNV-1a, 64-bit. The same function the token sandbox uses for its
/// capability SIDs; not a security boundary here, only a short stable name.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    #[test]
    fn names_fit_windows_limits_and_carry_the_role() {
        let n = OwnerNames::derive(SID, None).unwrap();
        for role in Role::FIXED {
            let user = n.user(role);
            assert!(user.len() <= MAX_USER_NAME, "{user} is too long");
            assert!(user.starts_with(&format!("kgraf-{}-", role.short())));
            // Characters a local user name refuses.
            assert!(!user.contains(|c: char| "\"/\\[]:;|=,+*?<>@ ".contains(c)));
            assert_eq!(n.role_of(&user), Some(role));
        }
        assert_ne!(n.user(Role::Offline), n.user(Role::Online));
        assert!(n.group().starts_with("kgraf-sandbox-"));
        assert_eq!(n.rules().len(), RulePurpose::ALL.len());
    }

    #[test]
    fn names_are_stable_and_differ_by_owner_and_tag() {
        let a = OwnerNames::derive(SID, None).unwrap();
        assert_eq!(a, OwnerNames::derive(SID, None).unwrap());
        let other = OwnerNames::derive("S-1-5-21-1-2-3-1002", None).unwrap();
        assert_ne!(a.user(Role::Offline), other.user(Role::Offline));
        let tagged = OwnerNames::derive(SID, Some("wintest")).unwrap();
        assert_ne!(a.user(Role::Offline), tagged.user(Role::Offline));
        assert_ne!(a.group(), tagged.group());
        // A fixed value, so a change to the derivation is seen in review: the
        // names on an owner's machine must not move between releases.
        assert_eq!(a.user(Role::Offline), format!("kgraf-off-{}", a.hash));
        assert_eq!(a.hash.len(), 8);
        assert_eq!(a.hash, format!("{:08x}", fnv1a64(SID.as_bytes()) as u32));
    }

    #[test]
    fn bad_sids_and_tags_are_refused() {
        for bad in [
            "",
            "S-1-",
            "S-1-5-",
            "S-1-5--21",
            "s-1-5-21",
            "S-1-5-21-x",
            "kgraf",
        ] {
            assert!(OwnerNames::derive(bad, None).is_err(), "{bad:?}");
        }
        for bad in ["", "Upper", "has space", "toolongtagvalue", "a-b"] {
            assert_eq!(
                OwnerNames::derive(SID, Some(bad)),
                Err(NameError::BadTag(bad.to_owned()))
            );
        }
        assert!(OwnerNames::derive("S-1-5-18", Some("t1")).is_ok());
    }

    #[test]
    fn comments_mark_the_owner_and_the_role() {
        let n = OwnerNames::derive(SID, None).unwrap();
        let c = n.user_comment(Role::Offline);
        assert!(c.contains(SID));
        assert!(n.owns_user_comment(Role::Offline, &c));
        assert!(!n.owns_user_comment(Role::Online, &c));
        assert!(!n.owns_user_comment(Role::Offline, ""));
        let other = OwnerNames::derive("S-1-5-21-1-2-3-1002", None).unwrap();
        assert!(!other.owns_user_comment(Role::Offline, &c));
        assert!(n.owns_group_comment(&n.group_comment()));
        assert!(!other.owns_group_comment(&n.group_comment()));
    }

    #[test]
    fn forbidden_password_parts_are_the_long_name_parts() {
        let n = OwnerNames::derive(SID, None).unwrap();
        let parts = n.password_forbidden_parts(Role::Offline);
        assert!(parts.contains(&"kgraf".to_owned()));
        assert!(parts.contains(&"off".to_owned()));
        assert!(parts.contains(&n.hash));
        // "on" is 2 characters: the policy ignores it.
        let online = n.password_forbidden_parts(Role::Online);
        assert!(!online.contains(&"on".to_owned()));
        // One account's password is judged against its own name only.
        assert!(!online.contains(&"off".to_owned()));
    }

    #[test]
    fn slot_user_names_fit_a_windows_user_name_and_are_distinct() {
        let names = OwnerNames::derive(SID, None).unwrap().with_slots(MAX_SLOTS);
        let all = names.users();
        assert_eq!(all.len(), 2 + MAX_SLOTS as usize);
        let unique: std::collections::BTreeSet<&String> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "every name is its own");
        for name in &all {
            assert!(
                name.len() <= MAX_USER_NAME,
                "{name} is {} characters",
                name.len()
            );
            assert!(name.starts_with("kgraf-"));
        }
        assert_eq!(
            names.user(Role::Slot(0)),
            format!("kgraf-s00-{}", names.hash)
        );
        assert_eq!(
            names.user(Role::Slot(7)),
            format!("kgraf-s07-{}", names.hash)
        );
    }

    #[test]
    fn slot_roles_round_trip_through_their_keys_and_refuse_anything_else() {
        for i in [0u8, 9, 10, MAX_SLOTS - 1] {
            let role = Role::Slot(i);
            assert_eq!(Role::from_key(&role.key()), Some(role));
            assert!(role.blocked(), "a slot user's network is blocked");
        }
        assert!(Role::Offline.blocked());
        assert!(!Role::Online.blocked());
        for bad in [
            "slot", "slot0", "slot000", "slot1x", "slotxx", "slot-1", "Slot00", "",
        ] {
            assert_eq!(Role::from_key(bad), None, "{bad:?}");
        }
        assert_eq!(Role::from_key(&format!("slot{MAX_SLOTS:02}")), None);
    }

    #[test]
    fn the_role_of_a_name_is_found_whatever_this_setups_slot_count_is() {
        // A removal must recognize a user that an earlier, larger setup made.
        let small = OwnerNames::derive(SID, None).unwrap().with_slots(1);
        let large = OwnerNames::derive(SID, None).unwrap().with_slots(8);
        let seventh = large.user(Role::Slot(7));
        assert_eq!(small.role_of(&seventh), Some(Role::Slot(7)));
        assert_eq!(small.roles().len(), 3, "its own plan covers one slot");
        assert_eq!(small.role_of("kgraf-off-00000000"), None, "another owner");
    }

    #[test]
    fn a_password_may_not_hold_a_part_of_its_own_accounts_name() {
        let names = OwnerNames::derive(SID, None).unwrap().with_slots(4);
        for role in names.roles() {
            let parts = names.password_forbidden_parts(role);
            assert!(parts.iter().any(|p| p == "kgraf"), "{role}: {parts:?}");
            assert!(parts.iter().any(|p| p == &names.hash), "{role}: {parts:?}");
            // Its own short part, and no other user's.
            let own = role.short();
            if own.len() >= 3 {
                assert!(parts.contains(&own), "{role}: {parts:?}");
            }
            let others: Vec<String> = names
                .roles()
                .into_iter()
                .filter(|r| *r != role)
                .map(|r| r.short())
                .filter(|s| s.len() >= 3)
                .collect();
            for other in others {
                assert!(!parts.contains(&other), "{role}: {parts:?}");
            }
        }
    }

    #[test]
    fn a_worktree_keeps_one_slot_and_every_slot_is_used() {
        let roots: Vec<std::path::PathBuf> = (0..200)
            .map(|i| std::path::PathBuf::from(format!(r"C:\work\tree-{i:03}")))
            .collect();
        for count in [1u8, 2, 4, 8] {
            let mut seen = vec![0usize; usize::from(count)];
            for root in &roots {
                let slot = slot_of(root, count);
                assert!(slot < count, "{slot} is not below {count}");
                assert_eq!(slot, slot_of(root, count), "one folder, one slot");
                seen[usize::from(slot)] += 1;
            }
            // No user sits idle while another holds every worktree.
            assert!(seen.iter().all(|n| *n > 0), "count {count}: {seen:?}");
        }
    }

    #[test]
    fn the_slot_does_not_depend_on_how_the_path_was_written() {
        let count = 4;
        let lower = std::path::PathBuf::from(r"c:\work\tree");
        let upper = std::path::PathBuf::from(r"C:\Work\Tree");
        assert_eq!(slot_of(&lower, count), slot_of(&upper, count));
        assert_ne!(
            slot_of(&std::path::PathBuf::from(r"C:\work\a"), 97),
            slot_of(&std::path::PathBuf::from(r"C:\work\b"), 97)
        );
    }
}
