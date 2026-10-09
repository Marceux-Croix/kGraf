//! The DNS filter of the sandbox user setup (decision 0097): one Windows
//! Filtering Platform (WFP) filter that makes the DNS Client service refuse
//! name lookups from the users whose network is blocked.
//!
//! A name lookup does not leave the calling process as a packet. The process
//! asks the DNS Client service (Dnscache) over local RPC, and the service
//! sends the query as NETWORK SERVICE. The firewall rules match the calling
//! user's SID, so they never see that query. The filter here sits on the
//! user-mode RPC layer (`FWPM_LAYER_RPC_UM`) instead: it blocks a call to
//! the DNS Client's RPC interface when the caller's token is one of the
//! blocked users. The direct path, a query the process sends itself, is
//! already blocked by the firewall rules.
//!
//! ```text
//!   offline or slot user process          Dnscache (NETWORK SERVICE)
//!     getaddrinfo / DnsQuery --RPC------> [RPC_UM filter: interface and
//!                                          caller token match -> block]
//!     direct UDP 53 -------X firewall rule (scoped to the user's SID)
//! ```
//!
//! Everything here is platform-neutral: the keys, the names, the matching
//! security descriptor and the interface identifier are tested on every
//! host. The WFP calls are in `windows::wfp`.
//!
//! **Keys.** WFP names its objects by GUID. The provider, sublayer and filter
//! keys are derived from the owner's SID and the tag, as the user names are,
//! so two owners on one machine, or a tagged test setup and a real one, never
//! share an object, and `kgraf sandbox remove` deletes exactly the keys its
//! record lists and no other.

use std::fmt;

use super::names::{OwnerNames, fnv1a64, is_sid_string};

/// The DNS Client service's RPC interface (`DnsResolver`, version 2.0, on the
/// `ncalrpc` endpoint `DNSResolver`), as `0x<8>_<4>_<4>_<4>_<12>` of
/// `45776b01-5956-4485-9f80-f428f7d60129`.
///
/// The value is taken from the ReactOS reimplementation of the interface
/// (`dnsrslvr.idl`). It is not confirmed against Windows' own `dnsrslvr.dll`:
/// a wrong value blocks nothing and fails silently, which is why
/// `kgraf sandbox selfcheck` judges the filter by its effect and never by its
/// presence.
pub const DNS_RESOLVER_INTERFACE: u128 = 0x45776b01_5956_4485_9f80_f428f7d60129;

/// `FWP_ACTRL_MATCH_FILTER`: the access right a WFP user condition checks.
/// A token that the descriptor grants this right matches the condition.
pub const MATCH_FILTER_ACCESS: u32 = 0x0000_0001;

/// The sublayer's weight, the middle of the range, as codex uses.
pub const SUBLAYER_WEIGHT: u16 = 0x8000;

/// One of the three WFP objects the setup makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WfpObject {
    /// kGraf's provider: the filter and the sublayer name it as theirs.
    Provider,
    /// kGraf's sublayer, which holds the filter.
    Sublayer,
    /// The filter that blocks the DNS Client interface for blocked users.
    Filter,
}

impl WfpObject {
    /// In the order they must be added. Removal runs in the reverse order:
    /// a provider or sublayer that an object still names cannot be deleted.
    pub const ALL: [WfpObject; 3] = [WfpObject::Provider, WfpObject::Sublayer, WfpObject::Filter];

    /// The word in the undo record: `wfp-provider`, `wfp-sublayer`,
    /// `wfp-filter`.
    pub fn key(self) -> &'static str {
        match self {
            WfpObject::Provider => "wfp-provider",
            WfpObject::Sublayer => "wfp-sublayer",
            WfpObject::Filter => "wfp-filter",
        }
    }

    pub fn from_key(s: &str) -> Option<WfpObject> {
        WfpObject::ALL.into_iter().find(|o| o.key() == s)
    }

    /// What the object is, in words.
    pub fn noun(self) -> &'static str {
        match self {
            WfpObject::Provider => "WFP provider",
            WfpObject::Sublayer => "WFP sublayer",
            WfpObject::Filter => "WFP filter",
        }
    }
}

impl fmt::Display for WfpObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.noun())
    }
}

/// The keys of one owner's WFP objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WfpKeys {
    pub provider: u128,
    pub sublayer: u128,
    pub filter: u128,
}

impl WfpKeys {
    /// The keys for `names`. They depend on the owner's SID and the tag only,
    /// not on the slot count, so a setup with more slots refreshes the same
    /// filter.
    pub fn derive(names: &OwnerNames) -> Self {
        let key = |o: WfpObject| derive_key(names.owner_sid(), names.tag(), o);
        Self {
            provider: key(WfpObject::Provider),
            sublayer: key(WfpObject::Sublayer),
            filter: key(WfpObject::Filter),
        }
    }

    pub fn get(&self, object: WfpObject) -> u128 {
        match object {
            WfpObject::Provider => self.provider,
            WfpObject::Sublayer => self.sublayer,
            WfpObject::Filter => self.filter,
        }
    }

    /// Each object with its key, in the order they must be added.
    pub fn all(&self) -> [(WfpObject, u128); 3] {
        WfpObject::ALL.map(|o| (o, self.get(o)))
    }
}

/// A stable GUID for one object of one owner: 128 bits of FNV-1a over the
/// owner's SID, the tag and the object, marked as an RFC 9562 version 8
/// (vendor-defined) GUID. Not a security boundary: a collision with an object
/// another program made would only let `remove` try to delete a key the
/// record lists, and the record is written only by kGraf's elevated step.
fn derive_key(owner_sid: &str, tag: Option<&str>, object: WfpObject) -> u128 {
    let input = format!(
        "kgraf-sandbox-wfp/{}/{owner_sid}/{}",
        object.key(),
        tag.unwrap_or("-")
    );
    let hi = fnv1a64(input.as_bytes());
    let lo = fnv1a64(format!("{input}/low").as_bytes());
    let mut v = (u128::from(hi) << 64) | u128::from(lo);
    // Version 8 in the top four bits of the third group.
    v = (v & !(0xF_u128 << 76)) | (0x8_u128 << 76);
    // Variant 10 in the top two bits of the fourth group.
    v = (v & !(0x3_u128 << 62)) | (0x2_u128 << 62);
    v
}

/// The GUID `v` in its usual text form, lowercase:
/// `45776b01-5956-4485-9f80-f428f7d60129`.
pub fn guid_text(v: u128) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 64) as u16,
        (v >> 48) as u16,
        v & 0xFFFF_FFFF_FFFF
    )
}

/// The GUID in `s`, written as [`guid_text`] writes it (either case), or
/// `None`.
pub fn parse_guid(s: &str) -> Option<u128> {
    let groups: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    if groups.len() != lens.len()
        || groups
            .iter()
            .zip(lens)
            .any(|(g, n)| g.len() != n || !g.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    u128::from_str_radix(&groups.concat(), 16).ok()
}

/// The 16 bytes of the GUID `v` as Windows lays a `GUID` out in memory: the
/// first three groups little-endian, the last eight bytes as written. This is
/// the form of an `FWP_BYTE_ARRAY16` condition value for an interface UUID.
pub fn guid_bytes(v: u128) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&((v >> 96) as u32).to_le_bytes());
    out[4..6].copy_from_slice(&((v >> 80) as u16).to_le_bytes());
    out[6..8].copy_from_slice(&((v >> 64) as u16).to_le_bytes());
    out[8..16].copy_from_slice(&(v as u64).to_be_bytes());
    out
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MatchError {
    #[error("the DNS filter needs at least one user to match")]
    NoUsers,
    #[error("{0:?} is not a SID in string form")]
    BadSid(String),
}

/// The users the DNS filter names, by user name: the users the firewall rules
/// block ([`OwnerNames::blocked_users`]), never an online user or the owner.
/// Setup resolves these to the SIDs it passes to [`match_sddl`].
pub fn filter_users(names: &OwnerNames) -> Vec<String> {
    names.blocked_users()
}

/// The security descriptor, in SDDL, that the filter's caller-token condition
/// (`FWPM_CONDITION_REMOTE_USER_TOKEN`) checks: one allow entry for
/// [`MATCH_FILTER_ACCESS`] (`CC` in SDDL) per user whose lookups must be
/// refused. A token that holds none of these SIDs is granted nothing, so the
/// filter does not match it: the owner and the online users resolve names as
/// before.
///
/// One entry per user SID, never the sandbox group's SID: the group also
/// holds the online users, which need lookups for dependency installs.
/// Owner and group are SYSTEM, because an access check refuses a descriptor
/// without them.
pub fn match_sddl(sids: &[&str]) -> Result<String, MatchError> {
    if sids.is_empty() {
        return Err(MatchError::NoUsers);
    }
    let mut out = String::from("O:SYG:SYD:");
    for sid in sids {
        if !is_sid_string(sid) {
            return Err(MatchError::BadSid((*sid).to_owned()));
        }
        out.push_str(&format!("(A;;CC;;;{sid})"));
    }
    Ok(out)
}

/// The provider's or sublayer's name, as `netsh wfp show filters` and WFP
/// tools list it.
pub fn display_name(names: &OwnerNames, object: WfpObject) -> String {
    match object {
        WfpObject::Provider => format!("kGraf sandbox ({})", names.group()),
        WfpObject::Sublayer => format!("kGraf sandbox filters ({})", names.group()),
        WfpObject::Filter => format!(
            "kGraf sandbox: no DNS lookups for offline users ({})",
            names.group()
        ),
    }
}

/// The provider's, sublayer's or filter's description.
pub fn description(object: WfpObject) -> &'static str {
    match object {
        WfpObject::Provider => {
            "Made by kgraf sandbox setup; kgraf sandbox remove deletes it. Owns the filter \
             that keeps kGraf's offline sandbox users from name lookups."
        }
        WfpObject::Sublayer => "Holds kGraf's sandbox filters.",
        WfpObject::Filter => {
            "Blocks calls to the DNS Client service's RPC interface from kGraf's offline \
             and worker slot sandbox users, so their name lookups cannot leave the machine."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn the_interface_constant_is_the_dns_resolver_uuid() {
        assert_eq!(
            guid_text(DNS_RESOLVER_INTERFACE),
            "45776b01-5956-4485-9f80-f428f7d60129"
        );
        // The in-memory layout of a Windows GUID: the first three groups
        // byte-swapped, the rest as written.
        assert_eq!(
            guid_bytes(DNS_RESOLVER_INTERFACE),
            [
                0x01, 0x6b, 0x77, 0x45, 0x56, 0x59, 0x85, 0x44, 0x9f, 0x80, 0xf4, 0x28, 0xf7, 0xd6,
                0x01, 0x29
            ]
        );
    }

    #[test]
    fn guids_round_trip_through_text() {
        for v in [0, u128::MAX, DNS_RESOLVER_INTERFACE] {
            assert_eq!(parse_guid(&guid_text(v)), Some(v));
        }
        assert_eq!(
            parse_guid("45776B01-5956-4485-9F80-F428F7D60129"),
            Some(DNS_RESOLVER_INTERFACE)
        );
        for bad in [
            "",
            "45776b01-5956-4485-9f80",
            "45776b0-15956-4485-9f80-f428f7d60129",
            "45776b01-5956-4485-9f80-f428f7d6012g",
            "{45776b01-5956-4485-9f80-f428f7d60129}",
            "45776b01-5956-4485-9f80-f428f7d60129-",
        ] {
            assert_eq!(parse_guid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn keys_are_stable_distinct_and_per_owner_and_tag() {
        let n = OwnerNames::derive(OWNER, None).unwrap();
        let k = WfpKeys::derive(&n);
        assert_eq!(k, WfpKeys::derive(&n));
        assert_eq!(k, WfpKeys::derive(&n.clone().with_slots(9)));
        let all: std::collections::BTreeSet<u128> = k.all().iter().map(|(_, v)| *v).collect();
        assert_eq!(all.len(), 3, "three distinct keys");
        let other = WfpKeys::derive(&OwnerNames::derive("S-1-5-21-1-2-3-1002", None).unwrap());
        let tagged = WfpKeys::derive(&OwnerNames::derive(OWNER, Some("wintest")).unwrap());
        for (o, v) in k.all() {
            assert_ne!(v, other.get(o), "{o} differs by owner");
            assert_ne!(v, tagged.get(o), "{o} differs by tag");
            let text = guid_text(v);
            assert_eq!(&text[14..15], "8", "version 8: {text}");
            assert!(matches!(&text[19..20], "8" | "9" | "a" | "b"), "{text}");
        }
    }

    #[test]
    fn the_match_descriptor_names_each_user_and_nothing_else() {
        let sddl = match_sddl(&["S-1-5-21-1-2-3-1010", "S-1-5-21-1-2-3-1012"]).unwrap();
        assert_eq!(
            sddl,
            "O:SYG:SYD:(A;;CC;;;S-1-5-21-1-2-3-1010)(A;;CC;;;S-1-5-21-1-2-3-1012)"
        );
        // Only allow entries for the match right: no deny, no other right,
        // no well-known group that would take in the owner.
        assert_eq!(sddl.matches("(A;;CC;;;").count(), 2);
        assert_eq!(match_sddl(&[]), Err(MatchError::NoUsers));
        assert_eq!(
            match_sddl(&["S-1-5-21-1", "BU"]),
            Err(MatchError::BadSid("BU".into()))
        );
    }

    #[test]
    fn names_say_kgraf_and_object_keys_round_trip() {
        let n = OwnerNames::derive(OWNER, None).unwrap();
        for o in WfpObject::ALL {
            assert!(display_name(&n, o).starts_with("kGraf sandbox"));
            assert!(display_name(&n, o).contains(&n.group()));
            assert!(description(o).contains("kGraf"));
            assert_eq!(WfpObject::from_key(o.key()), Some(o));
        }
        assert_eq!(WfpObject::from_key("wfp"), None);
    }

    /// The DNS filter names exactly the users the firewall rules block: the
    /// fixed offline user and every slot user without the network, never an
    /// online user or the owner. Checked by name, and by SID through the two
    /// descriptors setup writes, for one slot, the default four and the
    /// maximum.
    #[test]
    fn the_dns_filter_blocks_exactly_the_firewall_rules_users() {
        use super::super::firewall::{offline_rules, rule_users};
        use super::super::names::{MAX_SLOTS, Role};

        for slots in [1, 4, MAX_SLOTS] {
            let n = OwnerNames::derive(OWNER, None).unwrap().with_slots(slots);
            let dns = filter_users(&n);
            assert_eq!(dns, rule_users(&n), "{slots} slots");
            let mut expected = vec![n.user(Role::Offline)];
            expected.extend((0..slots).map(|i| n.user(Role::Slot(i))));
            assert_eq!(dns, expected, "{slots} slots");
            let online: Vec<String> = std::iter::once(n.user(Role::Online))
                .chain((0..slots).map(|i| n.user(Role::OnlineSlot(i))))
                .collect();
            for user in &online {
                assert!(!dns.contains(user), "{user} is online: {slots} slots");
            }

            // A stand-in SID per user, as setup resolves them.
            let sid_of = |user: &String| {
                let at = n.users().iter().position(|u| u == user).unwrap();
                format!("S-1-5-21-7-7-7-{}", 2000 + at)
            };
            let dns_sids: Vec<String> = dns.iter().map(sid_of).collect();
            let refs: Vec<&str> = dns_sids.iter().map(String::as_str).collect();
            let sddl = match_sddl(&refs).unwrap();
            let rule_sids: Vec<String> = rule_users(&n).iter().map(sid_of).collect();
            let rule_refs: Vec<&str> = rule_sids.iter().map(String::as_str).collect();
            for rule in offline_rules(&n, &rule_refs) {
                assert_eq!(rule.user_sids, dns_sids, "{slots} slots");
            }
            assert_eq!(sddl.matches("(A;;CC;;;").count(), dns_sids.len());
            for user in &online {
                assert!(!sddl.contains(&format!(";{})", sid_of(user))), "{user}");
            }
            assert!(!sddl.contains(OWNER), "the owner is never filtered");
        }
    }
}
