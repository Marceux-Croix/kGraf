//! The DNS filter of the sandbox user setup (decision 0097): the Windows
//! Filtering Platform (WFP) filters meant to make the DNS Client service
//! refuse name lookups from the users whose network is blocked.
//!
//! A name lookup does not leave the calling process as a packet. The process
//! asks the DNS Client service (Dnscache) over local RPC, and the service
//! sends the query as NETWORK SERVICE. The firewall rules match the calling
//! user's SID, so they never see that query. The filters here sit on the
//! layers that do see the caller:
//!
//! * **The name resolution cache layers**
//!   (`FWPM_LAYER_NAME_RESOLUTION_CACHE_V4` and `..._V6`), one filter each,
//!   with the condition `FWPM_CONDITION_ALE_USER_ID`, "the identification of
//!   the local user". This is the only layer pair at which WFP is handed the
//!   local caller of a name lookup, so it is the only one that can reach a
//!   lookup the DNS Client service serves over local RPC.
//! * **The user-mode RPC layer** (`FWPM_LAYER_RPC_UM`), with the RPC
//!   interface and `FWPM_CONDITION_REMOTE_USER_TOKEN`. That layer is reached
//!   only for a remote DCE/RPC caller, never for a local ALPC call, so it
//!   cannot cover the local path; it is kept for a caller that reaches the
//!   interface over a network transport, and only where Windows takes it
//!   without an error.
//!
//! ```text
//!   offline or slot user process          Dnscache (NETWORK SERVICE)
//!     getaddrinfo / DnsQuery --ALPC-----> [NAME_RESOLUTION_CACHE_V4/V6
//!                                          filters: ALE_USER_ID matches
//!                                          a blocked user -> block]
//!     direct UDP 53 -------X firewall rule (scoped to the user's SID)
//!   a remote DCE/RPC caller ----------->  [RPC_UM filter: interface and
//!                                          remote token match -> block]
//! ```
//!
//! **Windows may refuse the name resolution cache filter.** On Windows
//! Server 2025 (24H2, build 26100) `FwpmFilterAdd0` refused the IPv4 filter
//! with `FWP_E_OUT_OF_BOUNDS` (0x80320028), after the RPC filter had been
//! taken in the same transaction (the abort then rolled the RPC filter back
//! too). openai/codex records the same refusal for "ordinary static filter
//! shapes" at these layers and leaves them out. What is known about the
//! cause: the RPC filter took the same descriptor through a condition of the
//! same data type (`FWP_SECURITY_DESCRIPTOR_TYPE`) in the same sublayer, so
//! neither the descriptor's bytes nor the sublayer weight explains it; every
//! filter leaves its own weight to the engine (`FWP_EMPTY`); and the engine
//! has distinct codes for a condition the layer does not have
//! (`FWP_E_CONDITION_NOT_FOUND`), a value of the wrong type
//! (`FWP_E_TYPE_MISMATCH`), an action (`FWP_E_INVALID_ACTION_TYPE`) and a
//! weight (`FWP_E_INVALID_WEIGHT`), and returned none of them. Whether the
//! layer refuses the user condition, the block action, or every static
//! filter is not known: when a layer refuses kGraf's filter, setup adds two
//! probe filters in a transaction it always aborts and reports what they
//! show ([`probe_reading`]).
//!
//! A refused filter does not stop setup: each filter is added in its own
//! transaction, setup goes on without the refused one, records it with its
//! error, and ends with a notice that DNS is not confined on this machine;
//! `kgraf sandbox status` shows it as a failed line. A filter that is added
//! is still not proved: whether a block at the name resolution cache layers
//! makes the lookup fail, or only keeps the service from its cache while the
//! query still leaves the machine, is not documented. Microsoft describes
//! the layer in one sentence, "This filtering layer allows for querying the
//! names recently resolved by the system". `kgraf sandbox selfcheck` judges
//! the filters by their effect, never by their presence.
//!
//! Everything here is platform-neutral: the keys, the names, the matching
//! security descriptor, the interface identifier and the reading of a
//! refusal are tested on every host. The WFP calls are in `windows::wfp`.
//!
//! **Keys.** WFP names its objects by GUID. The provider, sublayer and the
//! three filter keys are each derived from the owner's SID and the tag, as
//! the user names are, so two owners on one machine, or a tagged test setup
//! and a real one, never share an object, and `kgraf sandbox remove` deletes
//! exactly the keys its record lists and no other.

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

/// The sublayer's weight: the maximum, so nothing is decided before kGraf's
/// block.
///
/// Microsoft's "Filter Arbitration" page: "Network traffic traverses
/// sub-layers from the highest priority to the lowest priority", a permit
/// whose action right is cleared is a "hard permit" that "can be blocked at
/// another sub-layer only by a callout Veto", and "In order to silently
/// override 'Hard Permit' filters a firewall has to add its filters within a
/// higher priority sub-layer". `FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT` on the
/// block therefore stops only a *lower*-weight sublayer from permitting over
/// it; a third-party sublayer of higher weight with a hard permit is decided
/// first. The reuse source (openai/codex `windows-sandbox-rs`) uses 0x8000,
/// the middle of the range, and gives no reason; mid-range leaves half the
/// range above kGraf.
///
/// The maximum is right here because this sublayer holds block filters only.
/// A sublayer that can never permit cannot take traffic away from another
/// provider by being decided first: a block it does not match leaves the
/// action as it was, and a block it does match is what kGraf is for.
pub const SUBLAYER_WEIGHT: u16 = u16::MAX;

/// One of the WFP objects the setup makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WfpObject {
    /// kGraf's provider: the filters and the sublayer name it as theirs.
    Provider,
    /// kGraf's sublayer, which holds the filters.
    Sublayer,
    /// The filter on the user-mode RPC layer, for a remote caller of the DNS
    /// Client interface. It cannot see a local lookup (see the module
    /// header).
    RpcFilter,
    /// The filter on `FWPM_LAYER_NAME_RESOLUTION_CACHE_V4`, which does see
    /// the local caller.
    NameCacheFilterV4,
    /// The same on `FWPM_LAYER_NAME_RESOLUTION_CACHE_V6`.
    NameCacheFilterV6,
}

impl WfpObject {
    /// In the order they must be added. Removal runs in the reverse order:
    /// a provider or sublayer that an object still names cannot be deleted.
    pub const ALL: [WfpObject; 5] = [
        WfpObject::Provider,
        WfpObject::Sublayer,
        WfpObject::RpcFilter,
        WfpObject::NameCacheFilterV4,
        WfpObject::NameCacheFilterV6,
    ];

    /// Just the filters, in the order they are added.
    pub const FILTERS: [WfpObject; 3] = [
        WfpObject::RpcFilter,
        WfpObject::NameCacheFilterV4,
        WfpObject::NameCacheFilterV6,
    ];

    /// The word in the undo record: `wfp-provider`, `wfp-sublayer`,
    /// `wfp-filter`, `wfp-filter-name-cache-v4`, `wfp-filter-name-cache-v6`.
    ///
    /// The RPC filter keeps the plain `wfp-filter` word it had when it was
    /// the only filter, so a record an earlier setup wrote still parses and
    /// `kgraf sandbox remove` can still undo it.
    pub fn key(self) -> &'static str {
        match self {
            WfpObject::Provider => "wfp-provider",
            WfpObject::Sublayer => "wfp-sublayer",
            WfpObject::RpcFilter => "wfp-filter",
            WfpObject::NameCacheFilterV4 => "wfp-filter-name-cache-v4",
            WfpObject::NameCacheFilterV6 => "wfp-filter-name-cache-v6",
        }
    }

    pub fn from_key(s: &str) -> Option<WfpObject> {
        WfpObject::ALL.into_iter().find(|o| o.key() == s)
    }

    /// Whether the object is a filter, not the provider or the sublayer.
    pub fn is_filter(self) -> bool {
        WfpObject::FILTERS.contains(&self)
    }

    /// The layer a filter sits on, in words. Empty for the provider and the
    /// sublayer, which sit on no layer.
    pub fn layer_words(self) -> &'static str {
        match self {
            WfpObject::Provider | WfpObject::Sublayer => "",
            WfpObject::RpcFilter => "the user-mode RPC layer",
            WfpObject::NameCacheFilterV4 => "the name resolution cache layer, IPv4",
            WfpObject::NameCacheFilterV6 => "the name resolution cache layer, IPv6",
        }
    }

    /// What the object is, in words.
    pub fn noun(self) -> &'static str {
        match self {
            WfpObject::Provider => "WFP provider",
            WfpObject::Sublayer => "WFP sublayer",
            WfpObject::RpcFilter => "WFP filter (user-mode RPC layer)",
            WfpObject::NameCacheFilterV4 => "WFP filter (name resolution cache, IPv4)",
            WfpObject::NameCacheFilterV6 => "WFP filter (name resolution cache, IPv6)",
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
    pub rpc_filter: u128,
    pub name_cache_v4: u128,
    pub name_cache_v6: u128,
}

impl WfpKeys {
    /// The keys for `names`. They depend on the owner's SID and the tag only,
    /// not on the slot count, so a setup with more slots refreshes the same
    /// filters.
    pub fn derive(names: &OwnerNames) -> Self {
        let key = |o: WfpObject| derive_key(names.owner_sid(), names.tag(), o);
        Self {
            provider: key(WfpObject::Provider),
            sublayer: key(WfpObject::Sublayer),
            rpc_filter: key(WfpObject::RpcFilter),
            name_cache_v4: key(WfpObject::NameCacheFilterV4),
            name_cache_v6: key(WfpObject::NameCacheFilterV6),
        }
    }

    pub fn get(&self, object: WfpObject) -> u128 {
        match object {
            WfpObject::Provider => self.provider,
            WfpObject::Sublayer => self.sublayer,
            WfpObject::RpcFilter => self.rpc_filter,
            WfpObject::NameCacheFilterV4 => self.name_cache_v4,
            WfpObject::NameCacheFilterV6 => self.name_cache_v6,
        }
    }

    /// Each object with its key, in the order they must be added.
    pub fn all(&self) -> [(WfpObject, u128); 5] {
        WfpObject::ALL.map(|o| (o, self.get(o)))
    }

    /// Each filter with its key, in the order they are added.
    pub fn filters(&self) -> [(WfpObject, u128); 3] {
        WfpObject::FILTERS.map(|o| (o, self.get(o)))
    }
}

/// `FWPM_FILTER_FLAG_DISABLED`: the Base Filtering Engine has the filter, but
/// does not run it. A filter whose provider names no service gets this flag
/// when the engine cannot decide that the provider's service is running, and
/// Microsoft's own pages do not agree on whether that happens at every start,
/// which is the reason to read the flag instead of trusting either sentence.
pub const FILTER_DISABLED: u32 = 0x0000_0020;

/// Whether `flags`, an `FWPM_FILTER0::flags`, says the filter does not run.
pub fn filter_is_disabled(flags: u32) -> bool {
    flags & FILTER_DISABLED != 0
}

/// What a read of one owner's WFP objects found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WfpState {
    /// The keys that exist.
    pub present: std::collections::BTreeSet<u128>,
    /// The filter keys that exist and carry [`FILTER_DISABLED`]. Always a
    /// subset of `present`: present is not effective.
    pub disabled: std::collections::BTreeSet<u128>,
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

/// `FWP_E_OUT_OF_BOUNDS`: "The value is out of bounds." What
/// `FwpmFilterAdd0` returned for the name resolution cache filter on Windows
/// Server 2025.
pub const FWP_E_OUT_OF_BOUNDS: u32 = 0x8032_0028;

/// A WFP status as text: its number, and its name where it is one a DNS
/// filter has met or is likely to meet (values as `windows-sys` gives them).
pub fn wfp_code_text(code: u32) -> String {
    let name = match code {
        FWP_E_OUT_OF_BOUNDS => Some("FWP_E_OUT_OF_BOUNDS"),
        0x8032_0002 => Some("FWP_E_CONDITION_NOT_FOUND"),
        0x8032_0004 => Some("FWP_E_LAYER_NOT_FOUND"),
        0x8032_0009 => Some("FWP_E_ALREADY_EXISTS"),
        0x8032_000A => Some("FWP_E_IN_USE"),
        0x8032_0014 => Some("FWP_E_INCOMPATIBLE_LAYER"),
        0x8032_001E => Some("FWP_E_INVALID_FLAGS"),
        0x8032_0024 => Some("FWP_E_INVALID_ACTION_TYPE"),
        0x8032_0025 => Some("FWP_E_INVALID_WEIGHT"),
        0x8032_0027 => Some("FWP_E_TYPE_MISMATCH"),
        0x8007_0005 => Some("E_ACCESSDENIED"),
        _ => None,
    };
    match name {
        Some(n) => format!("WFP error 0x{code:08X} ({n})"),
        None => format!("WFP error 0x{code:08X}"),
    }
}

/// What two probe filters at a layer that refused kGraf's filter showed.
/// Each is a permit, which changes no traffic, added in a transaction that
/// is always aborted, so neither ever takes effect. `Err` carries the status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerProbe {
    /// A permit with no condition, at the same layer and sublayer, with the
    /// same weight as kGraf's filter.
    pub bare_permit: Result<(), u32>,
    /// A permit with kGraf's user condition (`FWPM_CONDITION_ALE_USER_ID`,
    /// the same descriptor), otherwise as `bare_permit`.
    pub user_permit: Result<(), u32>,
}

/// What a [`LayerProbe`] says about the cause of a refusal, in words for the
/// setup's notice and the undo record. Each answer names only what the two
/// probes separate: the layer itself, the user condition, or the rest of
/// kGraf's filter (the block action and its flags).
pub fn probe_reading(p: &LayerProbe) -> String {
    match (p.bare_permit, p.user_permit) {
        (Err(code), _) => format!(
            "probe: a permit with no condition at the same layer was refused too ({}), so \
             this layer takes no static filter of this shape and the user condition is not \
             the cause",
            wfp_code_text(code)
        ),
        (Ok(()), Err(code)) => format!(
            "probe: a permit with no condition at the same layer was taken and a permit with \
             the same user condition was refused ({}), so the cause is the user condition \
             (FWPM_CONDITION_ALE_USER_ID with this descriptor) at this layer",
            wfp_code_text(code)
        ),
        (Ok(()), Ok(())) => "probe: permits with and without the same user condition were \
                             taken at the same layer, so the cause is the block action or the \
                             flags of kGraf's filter"
            .to_owned(),
    }
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

/// The security descriptor, in SDDL, that the filters' user conditions check:
/// one allow entry for [`MATCH_FILTER_ACCESS`] (`CC` in SDDL) per user whose
/// lookups must be refused. A token that holds none of these SIDs is granted
/// nothing, so the filter does not match it: the owner and the online users
/// resolve names as before.
///
/// One descriptor serves every filter. `FWPM_CONDITION_ALE_USER_ID` (the name
/// resolution cache layers) and `FWPM_CONDITION_REMOTE_USER_TOKEN` (the
/// user-mode RPC layer) both take `FWP_SECURITY_DESCRIPTOR_TYPE`, so the same
/// bytes match the local caller on one layer and the remote caller on the
/// other.
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
        WfpObject::RpcFilter => format!(
            "kGraf sandbox: no DNS lookups for offline users, remote RPC ({})",
            names.group()
        ),
        WfpObject::NameCacheFilterV4 => format!(
            "kGraf sandbox: no DNS lookups for offline users, IPv4 ({})",
            names.group()
        ),
        WfpObject::NameCacheFilterV6 => format!(
            "kGraf sandbox: no DNS lookups for offline users, IPv6 ({})",
            names.group()
        ),
    }
}

/// The provider's, sublayer's or filter's description.
pub fn description(object: WfpObject) -> &'static str {
    match object {
        WfpObject::Provider => {
            "Made by kgraf sandbox setup; kgraf sandbox remove deletes it. Owns the filters \
             that keep kGraf's offline sandbox users from name lookups."
        }
        WfpObject::Sublayer => "Holds kGraf's sandbox filters.",
        WfpObject::RpcFilter => {
            "Blocks calls to the DNS Client service's RPC interface from kGraf's offline \
             and worker slot sandbox users. This layer sees a remote caller only; it does \
             not cover a lookup on this machine."
        }
        WfpObject::NameCacheFilterV4 => {
            "Blocks IPv4 name resolution for kGraf's offline and worker slot sandbox users, \
             so their name lookups cannot leave the machine."
        }
        WfpObject::NameCacheFilterV6 => {
            "Blocks IPv6 name resolution for kGraf's offline and worker slot sandbox users, \
             so their name lookups cannot leave the machine."
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
        assert_eq!(all.len(), 5, "five distinct keys");
        // Every filter has its own key, so one layer's filter can be read,
        // replaced or removed without touching another's.
        assert_eq!(k.filters().len(), 3);
        assert!(k.filters().iter().all(|(o, _)| o.is_filter()));
        assert!(!WfpObject::Provider.is_filter() && !WfpObject::Sublayer.is_filter());
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

    /// A filter the Base Filtering Engine has disabled is read like any
    /// other, so the flag is the only witness.
    /// Nothing is decided before a block-only sublayer: clearing the action
    /// right stops a lower-weight sublayer from permitting over the block,
    /// but a higher-weight sublayer with a hard permit is decided first.
    #[test]
    fn the_sublayer_weight_leaves_nothing_above_it() {
        // Not the reuse source's mid-range 0x8000, which leaves half the
        // range above kGraf.
        assert_eq!(SUBLAYER_WEIGHT, u16::MAX);
        assert_ne!(SUBLAYER_WEIGHT, 0x8000);
    }

    #[test]
    fn the_disabled_flag_is_read_from_the_filter_flags() {
        assert_eq!(FILTER_DISABLED, 0x20, "FWPM_FILTER_FLAG_DISABLED");
        assert!(filter_is_disabled(FILTER_DISABLED));
        // Persistent (0x01) and clear action right (0x08), the flags the
        // setup sets, with and without the disabled bit.
        assert!(!filter_is_disabled(0x01 | 0x08));
        assert!(filter_is_disabled(0x01 | 0x08 | FILTER_DISABLED));
        assert!(!filter_is_disabled(0));
        // A disabled key is always a present key too.
        let n = OwnerNames::derive(OWNER, None).unwrap();
        let k = WfpKeys::derive(&n);
        let state = WfpState {
            present: k.all().iter().map(|(_, v)| *v).collect(),
            disabled: [k.name_cache_v4].into_iter().collect(),
        };
        assert!(state.disabled.is_subset(&state.present));
        assert_eq!(WfpState::default(), WfpState::default());
        assert!(WfpState::default().present.is_empty());
    }

    /// Each probe result names one cause, and the refusal's code is written
    /// with its name.
    #[test]
    fn a_refusal_is_read_from_the_two_probes() {
        assert_eq!(
            wfp_code_text(FWP_E_OUT_OF_BOUNDS),
            "WFP error 0x80320028 (FWP_E_OUT_OF_BOUNDS)"
        );
        assert_eq!(wfp_code_text(0x8032_00FF), "WFP error 0x803200FF");
        let layer = probe_reading(&LayerProbe {
            bare_permit: Err(FWP_E_OUT_OF_BOUNDS),
            user_permit: Err(FWP_E_OUT_OF_BOUNDS),
        });
        assert!(layer.contains("no static filter"), "{layer}");
        assert!(layer.contains("not the cause"), "{layer}");
        assert!(layer.contains("FWP_E_OUT_OF_BOUNDS"), "{layer}");
        // The bare probe decides first: its refusal says the layer, whatever
        // the user probe did.
        assert_eq!(
            probe_reading(&LayerProbe {
                bare_permit: Err(FWP_E_OUT_OF_BOUNDS),
                user_permit: Ok(()),
            }),
            layer
        );
        let condition = probe_reading(&LayerProbe {
            bare_permit: Ok(()),
            user_permit: Err(FWP_E_OUT_OF_BOUNDS),
        });
        assert!(
            condition.contains("FWPM_CONDITION_ALE_USER_ID"),
            "{condition}"
        );
        let action = probe_reading(&LayerProbe {
            bare_permit: Ok(()),
            user_permit: Ok(()),
        });
        assert!(action.contains("block action"), "{action}");
        assert_ne!(layer, condition);
        assert_ne!(condition, action);
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
        // The RPC filter keeps the word it had when it was the only filter,
        // so a record an earlier setup wrote still parses.
        assert_eq!(
            WfpObject::from_key("wfp-filter"),
            Some(WfpObject::RpcFilter)
        );
        let words: std::collections::BTreeSet<&str> =
            WfpObject::ALL.iter().map(|o| o.key()).collect();
        assert_eq!(words.len(), WfpObject::ALL.len(), "distinct record words");
        // Each filter's display name says which layer it sits on, so an
        // administrator reading `netsh wfp show filters` can tell them apart.
        let v4 = display_name(&n, WfpObject::NameCacheFilterV4);
        let v6 = display_name(&n, WfpObject::NameCacheFilterV6);
        assert_ne!(v4, v6);
        assert_ne!(v4, display_name(&n, WfpObject::RpcFilter));
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
