//! The offline sandbox user's firewall rules, as data.
//!
//! Four block rules, each scoped to the offline user's SID through the
//! rule's local user list (`O:LSD:(A;;CC;;;<sid>)`), on every profile:
//!
//! | rule                 | direction | protocol | remote addresses | remote ports            |
//! |----------------------|-----------|----------|------------------|-------------------------|
//! | `block-outbound`     | out       | any      | not loopback     | any                     |
//! | `block-inbound`      | in        | any      | not loopback     | any                     |
//! | `block-loopback-tcp` | out       | TCP      | loopback         | all but [`PROXY_PORTS`] |
//! | `block-loopback-udp` | out       | UDP      | loopback         | any                     |
//!
//! So the offline user reaches nothing off the machine, and on the machine
//! only a TCP port in [`PROXY_PORTS`], where kGraf's egress proxy listens on
//! Windows. A block rule wins over any allow rule in Windows Firewall, so
//! another program's allow rule cannot open the network to this user.
//!
//! The address literals are codex's (`setup_provisioning/firewall.rs`,
//! Apache-2.0), which its tests show the firewall's COM interface accepts.
//! The loopback list here names `::1` alone where codex names `::/127`
//! (`::` and `::1`): `::` is not a loopback address and is already in the
//! non-loopback list.

use std::ops::RangeInclusive;

use super::names::OwnerNames;

/// The loopback TCP ports the offline user may reach: where kGraf's egress
/// proxy listens on Windows. Fixed, because a firewall rule is written once
/// at setup and the proxy's port is chosen at each run. Below the dynamic
/// port range Windows uses (49152 to 65535), so the system does not hand
/// these ports out as client ports, and clear of common development ports.
pub const PROXY_PORTS: RangeInclusive<u16> = 47100..=47131;

/// The remote addresses of the loopback rules.
pub const LOOPBACK_ADDRESSES: &str = "127.0.0.0/8,::1";

/// Every remote address but loopback: IPv4 without 127.0.0.0/8, IPv6
/// without `::1`.
pub const NON_LOOPBACK_ADDRESSES: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";

/// The group name shown in the firewall's rule list.
pub const RULE_GROUPING: &str = "kGraf sandbox";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RulePurpose {
    BlockOutbound,
    BlockInbound,
    BlockLoopbackTcp,
    BlockLoopbackUdp,
}

impl RulePurpose {
    pub const ALL: [RulePurpose; 4] = [
        RulePurpose::BlockOutbound,
        RulePurpose::BlockInbound,
        RulePurpose::BlockLoopbackTcp,
        RulePurpose::BlockLoopbackUdp,
    ];

    pub fn key(self) -> &'static str {
        match self {
            RulePurpose::BlockOutbound => "block-outbound",
            RulePurpose::BlockInbound => "block-inbound",
            RulePurpose::BlockLoopbackTcp => "block-loopback-tcp",
            RulePurpose::BlockLoopbackUdp => "block-loopback-udp",
        }
    }

    pub fn from_key(s: &str) -> Option<RulePurpose> {
        RulePurpose::ALL.into_iter().find(|p| p.key() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

/// IP protocol numbers as the firewall takes them (`NET_FW_IP_PROTOCOL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Any,
    Tcp,
    Udp,
}

impl Protocol {
    pub fn number(self) -> i32 {
        match self {
            Protocol::Any => 256,
            Protocol::Tcp => 6,
            Protocol::Udp => 17,
        }
    }
}

/// One block rule, ready for the firewall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSpec {
    pub purpose: RulePurpose,
    pub name: String,
    pub description: String,
    pub direction: Direction,
    pub protocol: Protocol,
    pub remote_addresses: &'static str,
    /// `None` is every port.
    pub remote_ports: Option<String>,
    /// The SID the rule applies to.
    pub user_sid: String,
    /// The rule's local user list, in SDDL.
    pub local_users: String,
}

/// The offline user's rules.
pub fn offline_rules(names: &OwnerNames, offline_sid: &str) -> Vec<RuleSpec> {
    let local_users = local_user_sddl(offline_sid);
    RulePurpose::ALL
        .into_iter()
        .map(|purpose| {
            let (direction, protocol, remote_addresses, remote_ports, what) = match purpose {
                RulePurpose::BlockOutbound => (
                    Direction::Out,
                    Protocol::Any,
                    NON_LOOPBACK_ADDRESSES,
                    None,
                    "blocks all outbound traffic off this machine",
                ),
                RulePurpose::BlockInbound => (
                    Direction::In,
                    Protocol::Any,
                    NON_LOOPBACK_ADDRESSES,
                    None,
                    "blocks all inbound traffic from off this machine",
                ),
                RulePurpose::BlockLoopbackTcp => (
                    Direction::Out,
                    Protocol::Tcp,
                    LOOPBACK_ADDRESSES,
                    Some(blocked_ports(&PROXY_PORTS)),
                    "blocks loopback TCP except kGraf's egress proxy ports",
                ),
                RulePurpose::BlockLoopbackUdp => (
                    Direction::Out,
                    Protocol::Udp,
                    LOOPBACK_ADDRESSES,
                    None,
                    "blocks loopback UDP",
                ),
            };
            RuleSpec {
                purpose,
                name: names.rule(purpose),
                description: format!(
                    "kGraf sandbox: {what} for the offline sandbox user {}",
                    names.user(super::names::Role::Offline)
                ),
                direction,
                protocol,
                remote_addresses,
                remote_ports,
                user_sid: offline_sid.to_owned(),
                local_users: local_users.clone(),
            }
        })
        .collect()
}

/// A local user list that names one SID: owner LocalSystem, and an allow
/// entry with the control access right for the SID, the form the firewall
/// reads.
pub fn local_user_sddl(sid: &str) -> String {
    format!("O:LSD:(A;;CC;;;{sid})")
}

/// Every port from 1 to 65535 outside `allowed`, as the firewall's port list
/// (`1-47099,47132-65535`).
pub fn blocked_ports(allowed: &RangeInclusive<u16>) -> String {
    let (lo, hi) = (u32::from(*allowed.start()), u32::from(*allowed.end()));
    let mut parts = Vec::new();
    let mut push = |a: u32, b: u32| {
        if a > b {
            return;
        }
        parts.push(if a == b {
            a.to_string()
        } else {
            format!("{a}-{b}")
        });
    };
    if allowed.is_empty() || lo == 0 && hi == 0 {
        push(1, 65535);
    } else {
        push(1, lo.max(1) - 1);
        push(hi + 1, 65535);
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const OWNER: &str = "S-1-5-21-1-2-3-1001";
    const OFFLINE: &str = "S-1-5-21-1-2-3-1010";

    /// One parsed entry of an address list.
    enum Entry {
        V4(u32, u32),
        V6(u128, u128),
    }

    fn parse_list(list: &str) -> Vec<Entry> {
        list.split(',')
            .map(|item| {
                if let Some((addr, bits)) = item.split_once('/') {
                    let bits: u32 = bits.parse().unwrap();
                    if let Ok(a) = addr.parse::<Ipv4Addr>() {
                        let a = u32::from(a);
                        let mask = if bits == 0 {
                            0
                        } else {
                            u32::MAX << (32 - bits)
                        };
                        return Entry::V4(a & mask, (a & mask) | !mask);
                    }
                    let a = u128::from(addr.parse::<Ipv6Addr>().unwrap());
                    let mask = if bits == 0 {
                        0
                    } else {
                        u128::MAX << (128 - bits)
                    };
                    return Entry::V6(a & mask, (a & mask) | !mask);
                }
                if let Some((a, b)) = item.split_once('-') {
                    if let (Ok(a), Ok(b)) = (a.parse::<Ipv4Addr>(), b.parse::<Ipv4Addr>()) {
                        return Entry::V4(a.into(), b.into());
                    }
                    let a: u128 = a.parse::<Ipv6Addr>().unwrap().into();
                    let b: u128 = b.parse::<Ipv6Addr>().unwrap().into();
                    return Entry::V6(a, b);
                }
                if let Ok(a) = item.parse::<Ipv4Addr>() {
                    return Entry::V4(a.into(), a.into());
                }
                let a: u128 = item.parse::<Ipv6Addr>().unwrap().into();
                Entry::V6(a, a)
            })
            .collect()
    }

    fn covers_v4(list: &str, ip: Ipv4Addr) -> bool {
        let ip = u32::from(ip);
        parse_list(list)
            .iter()
            .any(|e| matches!(e, Entry::V4(a, b) if *a <= ip && ip <= *b))
    }

    fn covers_v6(list: &str, ip: Ipv6Addr) -> bool {
        let ip = u128::from(ip);
        parse_list(list)
            .iter()
            .any(|e| matches!(e, Entry::V6(a, b) if *a <= ip && ip <= *b))
    }

    #[test]
    fn non_loopback_list_covers_everything_but_loopback() {
        let l = NON_LOOPBACK_ADDRESSES;
        for ip in ["127.0.0.1", "127.0.0.0", "127.255.255.255", "127.1.2.3"] {
            assert!(
                !covers_v4(l, ip.parse().unwrap()),
                "{ip} must stay reachable"
            );
        }
        for ip in [
            "0.0.0.0",
            "1.1.1.1",
            "10.0.0.1",
            "126.255.255.255",
            "128.0.0.0",
            "169.254.169.254",
            "192.168.1.1",
            "255.255.255.255",
        ] {
            assert!(covers_v4(l, ip.parse().unwrap()), "{ip} must be blocked");
        }
        assert!(!covers_v6(l, Ipv6Addr::LOCALHOST));
        for ip in [
            "::",
            "::2",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            assert!(covers_v6(l, ip.parse().unwrap()), "{ip} must be blocked");
        }
        // No entry in the list reaches into 127.0.0.0/8 at all.
        for e in parse_list(l) {
            if let Entry::V4(a, b) = e {
                assert!(
                    b < u32::from(Ipv4Addr::new(127, 0, 0, 0))
                        || a > u32::from(Ipv4Addr::new(127, 255, 255, 255))
                );
            }
        }
    }

    #[test]
    fn loopback_list_is_exactly_loopback() {
        let l = LOOPBACK_ADDRESSES;
        assert!(covers_v4(l, Ipv4Addr::new(127, 0, 0, 1)));
        assert!(covers_v4(l, Ipv4Addr::new(127, 255, 255, 255)));
        assert!(!covers_v4(l, Ipv4Addr::new(128, 0, 0, 0)));
        assert!(!covers_v4(l, Ipv4Addr::new(126, 255, 255, 255)));
        assert!(covers_v6(l, Ipv6Addr::LOCALHOST));
        assert!(!covers_v6(l, Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn port_complement_leaves_only_the_proxy_range_open() {
        assert_eq!(blocked_ports(&PROXY_PORTS), "1-47099,47132-65535");
        assert_eq!(blocked_ports(&(1..=1)), "2-65535");
        assert_eq!(blocked_ports(&(65535..=65535)), "1-65534");
        assert_eq!(blocked_ports(&(80..=80)), "1-79,81-65535");
        // Every port is either blocked or in the range, never both.
        let blocked = blocked_ports(&PROXY_PORTS);
        let ranges: Vec<(u32, u32)> = blocked
            .split(',')
            .map(|r| match r.split_once('-') {
                Some((a, b)) => (a.parse().unwrap(), b.parse().unwrap()),
                None => (r.parse().unwrap(), r.parse().unwrap()),
            })
            .collect();
        for port in 1..=65535u32 {
            let is_blocked = ranges.iter().any(|(a, b)| *a <= port && port <= *b);
            let in_range = PROXY_PORTS.contains(&(port as u16));
            assert_ne!(is_blocked, in_range, "port {port}");
        }
    }

    #[test]
    fn proxy_ports_sit_below_the_dynamic_range() {
        assert!(*PROXY_PORTS.end() < 49152);
        assert!(*PROXY_PORTS.start() > 1024);
        assert!(PROXY_PORTS.clone().count() >= 16);
    }

    #[test]
    fn offline_rules_scope_every_rule_to_the_offline_sid() {
        let names = OwnerNames::derive(OWNER, None).unwrap();
        let rules = offline_rules(&names, OFFLINE);
        assert_eq!(rules.len(), 4);
        let names_set: std::collections::BTreeSet<_> =
            rules.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names_set.len(), 4, "rule names are distinct");
        for r in &rules {
            assert_eq!(r.local_users, format!("O:LSD:(A;;CC;;;{OFFLINE})"));
            assert_eq!(r.user_sid, OFFLINE);
            assert!(!r.local_users.contains(OWNER), "the owner is never blocked");
            assert_eq!(r.name, names.rule(r.purpose));
            assert!(r.name.starts_with("kgraf-sandbox-"));
        }
        let by = |p| rules.iter().find(|r| r.purpose == p).unwrap();
        let out = by(RulePurpose::BlockOutbound);
        assert_eq!(
            (out.direction, out.protocol),
            (Direction::Out, Protocol::Any)
        );
        assert_eq!(out.remote_addresses, NON_LOOPBACK_ADDRESSES);
        assert_eq!(out.remote_ports, None);
        let inbound = by(RulePurpose::BlockInbound);
        assert_eq!(inbound.direction, Direction::In);
        let tcp = by(RulePurpose::BlockLoopbackTcp);
        assert_eq!(
            (tcp.protocol, tcp.remote_addresses),
            (Protocol::Tcp, LOOPBACK_ADDRESSES)
        );
        assert_eq!(tcp.remote_ports.as_deref(), Some("1-47099,47132-65535"));
        let udp = by(RulePurpose::BlockLoopbackUdp);
        assert_eq!(
            (udp.protocol, udp.remote_ports.as_deref()),
            (Protocol::Udp, None)
        );
        assert_eq!(Protocol::Any.number(), 256);
        assert_eq!(Protocol::Tcp.number(), 6);
        assert_eq!(Protocol::Udp.number(), 17);
    }

    #[test]
    fn purposes_round_trip_through_their_keys() {
        for p in RulePurpose::ALL {
            assert_eq!(RulePurpose::from_key(p.key()), Some(p));
        }
        assert_eq!(RulePurpose::from_key("allow-all"), None);
    }
}
