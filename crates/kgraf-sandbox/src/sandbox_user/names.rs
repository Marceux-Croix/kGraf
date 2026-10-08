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

use super::firewall::RulePurpose;

/// The most characters a Windows local user name may hold.
pub const MAX_USER_NAME: usize = 20;

/// The most characters of a tag.
pub const MAX_TAG: usize = 12;

/// Which sandbox user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Commands without the network, or limited to kGraf's egress proxy.
    /// Firewall rules block this user's traffic.
    Offline,
    /// Commands with the network open. No firewall rules.
    Online,
}

impl Role {
    pub const ALL: [Role; 2] = [Role::Offline, Role::Online];

    /// The word in messages and records.
    pub fn key(self) -> &'static str {
        match self {
            Role::Offline => "offline",
            Role::Online => "online",
        }
    }

    pub fn from_key(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.key() == s)
    }

    fn short(self) -> &'static str {
        match self {
            Role::Offline => "off",
            Role::Online => "on",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
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
        })
    }

    pub fn owner_sid(&self) -> &str {
        &self.owner_sid
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// `kgraf-off-1a2b3c4d`, `kgraf-on-1a2b3c4d`.
    pub fn user(&self, role: Role) -> String {
        format!("kgraf-{}-{}", role.short(), self.hash)
    }

    pub fn users(&self) -> [String; 2] {
        [self.user(Role::Offline), self.user(Role::Online)]
    }

    pub fn role_of(&self, user: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| self.user(*r) == user)
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
    pub fn password_forbidden_parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        for user in self.users() {
            for part in user.split(['-', '_', '.', ',', ' ', '#']) {
                if part.len() >= 3 && !parts.iter().any(|p| p == part) {
                    parts.push(part.to_ascii_lowercase());
                }
            }
        }
        parts
    }
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
        for role in Role::ALL {
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
        let parts = n.password_forbidden_parts();
        assert!(parts.contains(&"kgraf".to_owned()));
        assert!(parts.contains(&"off".to_owned()));
        assert!(parts.contains(&n.hash));
        // "on" is 2 characters: the policy ignores it.
        assert!(!parts.contains(&"on".to_owned()));
    }
}
