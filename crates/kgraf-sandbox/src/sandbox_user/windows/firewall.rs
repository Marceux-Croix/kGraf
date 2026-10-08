//! Windows Firewall rules for the sandbox users, through the firewall's COM
//! interface (`INetFwPolicy2`, `INetFwRule3`).
//!
//! Every rule is a block rule scoped to one sandbox user's SID with its local
//! user list (`LocalUserAuthorizedList`), on every profile. An update removes
//! the rule and adds it again from its [`RuleSpec`], so a rule that exists
//! always matches the specification exactly. Changing rules needs
//! administrator rights; reading the firewall's state does not.

use std::collections::BTreeSet;
use std::io;

use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule3, NET_FW_ACTION_BLOCK, NET_FW_MODIFY_STATE_OK, NET_FW_PROFILE2_ALL,
    NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC, NET_FW_RULE_DIR_IN,
    NET_FW_RULE_DIR_OUT, NetFwPolicy2, NetFwRule,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::BSTR;

use super::super::firewall::{Direction, Protocol, RULE_GROUPING, RuleSpec};
use super::super::plan::FirewallFacts;
use crate::error::SandboxError;

fn com_error(step: &'static str, e: windows::core::Error) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: io::Error::other(e.message()),
    }
}

/// The firewall policy object, with COM initialized on this thread.
fn policy() -> Result<INetFwPolicy2, SandboxError> {
    // SAFETY: initializes COM for this thread. S_FALSE (already initialized)
    // and RPC_E_CHANGED_MODE (initialized another way) both leave COM usable
    // for an in-process object, so the result is not an error here.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: creates the firewall policy object in process.
    unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| com_error("open the Windows Firewall policy", e))
}

/// Whether the firewall is on for every profile and local rules take effect.
/// A value that cannot be read is `None`.
pub fn facts() -> FirewallFacts {
    let Ok(p) = policy() else {
        return FirewallFacts::default();
    };
    let profiles = [
        NET_FW_PROFILE2_DOMAIN,
        NET_FW_PROFILE2_PRIVATE,
        NET_FW_PROFILE2_PUBLIC,
    ];
    let mut enabled = Some(true);
    for profile in profiles {
        // SAFETY: a read of one profile's state.
        match unsafe { p.get_FirewallEnabled(profile) } {
            Ok(v) if v == VARIANT_FALSE => enabled = Some(false),
            Ok(_) => {}
            Err(_) => {
                enabled = None;
                break;
            }
        }
    }
    // SAFETY: a read of whether local rules take effect.
    let local = unsafe { p.LocalPolicyModifyState() }
        .ok()
        .map(|s| s == NET_FW_MODIFY_STATE_OK);
    FirewallFacts {
        enabled_all_profiles: enabled,
        local_policy_effective: local,
    }
}

/// Which of `names` exist as firewall rules.
pub fn existing(names: &[String]) -> Result<BTreeSet<String>, SandboxError> {
    let p = policy()?;
    // SAFETY: reads the rule collection.
    let rules = unsafe { p.Rules() }.map_err(|e| com_error("list firewall rules", e))?;
    let mut found = BTreeSet::new();
    for name in names {
        // SAFETY: looks one rule up by name; a missing rule is an error result.
        if unsafe { rules.Item(&BSTR::from(name.as_str())) }.is_ok() {
            found.insert(name.clone());
        }
    }
    Ok(found)
}

/// Remove rule `name`. `false` when it did not exist.
pub fn remove(name: &str) -> Result<bool, SandboxError> {
    let p = policy()?;
    // SAFETY: reads the rule collection.
    let rules = unsafe { p.Rules() }.map_err(|e| com_error("list firewall rules", e))?;
    let n = BSTR::from(name);
    // SAFETY: a lookup by name.
    if unsafe { rules.Item(&n) }.is_err() {
        return Ok(false);
    }
    // SAFETY: removes the rule found above.
    unsafe { rules.Remove(&n) }.map_err(|e| com_error("remove a firewall rule", e))?;
    Ok(true)
}

/// Add `spec`, replacing a rule of the same name.
pub fn put(spec: &RuleSpec) -> Result<(), SandboxError> {
    remove(&spec.name)?;
    let p = policy()?;
    // SAFETY: reads the rule collection.
    let rules = unsafe { p.Rules() }.map_err(|e| com_error("list firewall rules", e))?;
    // SAFETY: creates an empty rule object in process.
    let rule: INetFwRule3 = unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| com_error("create a firewall rule", e))?;
    let set = |step: &'static str, r: windows::core::Result<()>| r.map_err(|e| com_error(step, e));
    // SAFETY: each call sets one property of the rule object created above.
    unsafe {
        set(
            "name a firewall rule",
            rule.SetName(&BSTR::from(spec.name.as_str())),
        )?;
        set(
            "describe a firewall rule",
            rule.SetDescription(&BSTR::from(spec.description.as_str())),
        )?;
        set(
            "group a firewall rule",
            rule.SetGrouping(&BSTR::from(RULE_GROUPING)),
        )?;
        set(
            "set a firewall rule's direction",
            rule.SetDirection(match spec.direction {
                Direction::Out => NET_FW_RULE_DIR_OUT,
                Direction::In => NET_FW_RULE_DIR_IN,
            }),
        )?;
        set(
            "set a firewall rule's protocol",
            rule.SetProtocol(spec.protocol.number()),
        )?;
        set(
            "set a firewall rule's remote addresses",
            rule.SetRemoteAddresses(&BSTR::from(spec.remote_addresses)),
        )?;
        if let Some(ports) = &spec.remote_ports
            && matches!(spec.protocol, Protocol::Tcp | Protocol::Udp)
        {
            set(
                "set a firewall rule's remote ports",
                rule.SetRemotePorts(&BSTR::from(ports.as_str())),
            )?;
        }
        set(
            "set a firewall rule's profiles",
            rule.SetProfiles(NET_FW_PROFILE2_ALL.0),
        )?;
        set(
            "set a firewall rule's local users",
            rule.SetLocalUserAuthorizedList(&BSTR::from(spec.local_users.as_str())),
        )?;
        set(
            "make a firewall rule block",
            rule.SetAction(NET_FW_ACTION_BLOCK),
        )?;
        set("enable a firewall rule", rule.SetEnabled(VARIANT_TRUE))?;
        set("add a firewall rule", rules.Add(&rule))?;
    }
    Ok(())
}
