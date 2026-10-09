//! The DNS filter's Windows Filtering Platform calls ([`super::super::wfp`]
//! holds the keys, names and descriptor, tested on every host).
//!
//! Setup adds three persistent objects in one transaction: kGraf's provider,
//! its sublayer and the filter. A re-run deletes the filter and adds it again
//! from the current list of blocked users, so a setup with more slots
//! refreshes it. Removal deletes each listed key and treats "not found" as
//! done. Changing objects needs administrator rights; reading them can too,
//! and a read that is refused is reported as unreadable, never as absent.
//!
//! **Reuse.** The engine and transaction guards, the provider and sublayer
//! setup and the filter layout are ported from openai/codex
//! `codex-rs/windows-sandbox-rs/src/wfp.rs` (Apache-2.0, read at commit
//! 665e5f45ab91d43ec3a49487fb9be90109dd2875). Differences: the layer is the
//! user-mode RPC layer, not the connect layers; the conditions are the RPC
//! interface and the caller's token, not the user ID and remote port; the
//! keys are per owner, not fixed; the block clears the action right, so a
//! permit in another sublayer cannot override it; the user match descriptor
//! is built from SDDL with one entry per user, not from one account name.
//! The `wfp` crate (mullvad/wfp-rs 0.2.0) was checked first: it builds no
//! RPC-layer filter and no security-descriptor condition.

use std::collections::BTreeSet;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, FWP_E_ALREADY_EXISTS, FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND,
    FWP_E_PROVIDER_NOT_FOUND, FWP_E_SUBLAYER_NOT_FOUND, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_BYTE_ARRAY16, FWP_BYTE_ARRAY16_TYPE, FWP_BYTE_BLOB, FWP_CONDITION_VALUE0,
    FWP_CONDITION_VALUE0_0, FWP_EMPTY, FWP_MATCH_EQUAL, FWP_SECURITY_DESCRIPTOR_TYPE, FWP_VALUE0,
    FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_REMOTE_USER_TOKEN, FWPM_CONDITION_RPC_IF_UUID,
    FWPM_DISPLAY_DATA0, FWPM_FILTER_CONDITION0, FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT,
    FWPM_FILTER_FLAG_PERSISTENT, FWPM_FILTER0, FWPM_FILTER0_0, FWPM_LAYER_RPC_UM,
    FWPM_PROVIDER_FLAG_PERSISTENT, FWPM_PROVIDER0, FWPM_SESSION0, FWPM_SUBLAYER_FLAG_PERSISTENT,
    FWPM_SUBLAYER0, FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0, FwpmFilterDeleteByKey0,
    FwpmFilterGetByKey0, FwpmFreeMemory0, FwpmProviderAdd0, FwpmProviderDeleteByKey0,
    FwpmProviderGetByKey0, FwpmSubLayerAdd0, FwpmSubLayerDeleteByKey0, FwpmSubLayerGetByKey0,
    FwpmTransactionAbort0, FwpmTransactionBegin0, FwpmTransactionCommit0,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::core::GUID;

use super::super::names::OwnerNames;
use super::super::wfp::{
    DNS_RESOLVER_INTERFACE, SUBLAYER_WEIGHT, WfpKeys, WfpObject, description, display_name,
    guid_bytes, match_sddl,
};
use super::wide;
use crate::error::SandboxError;

/// `E_ACCESSDENIED`, the form the WFP calls use for a refused access.
const E_ACCESSDENIED: u32 = 0x8007_0005;

/// How long a call waits for another program's WFP transaction to end.
const TRANSACTION_WAIT_MS: u32 = 10_000;

fn wfp_error(step: &'static str, code: u32) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: std::io::Error::other(format!("WFP error 0x{code:08X}")),
    }
}

fn check(code: u32, step: &'static str, allowed: &[u32]) -> Result<(), SandboxError> {
    if code == 0 || allowed.contains(&code) {
        Ok(())
    } else {
        Err(wfp_error(step, code))
    }
}

fn guid(v: u128) -> GUID {
    GUID::from_u128(v)
}

/// An open WFP engine handle, closed on drop.
struct Engine {
    handle: HANDLE,
}

impl Engine {
    /// Returns the raw status on failure, so a caller can tell a refused
    /// access from another failure.
    fn open() -> Result<Self, u32> {
        let name = wide("kGraf sandbox setup");
        // SAFETY: an all-zero FWPM_SESSION0 is the documented "no options"
        // session; the name outlives the call.
        let mut session: FWPM_SESSION0 = unsafe { std::mem::zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: name.as_ptr().cast_mut(),
            description: null_mut(),
        };
        session.txnWaitTimeoutInMSec = TRANSACTION_WAIT_MS;
        let mut handle: HANDLE = null_mut();
        // SAFETY: local engine (null server), default authentication, the
        // session above, and an out pointer to a handle.
        let code = unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_DEFAULT as u32,
                null(),
                &session,
                &mut handle,
            )
        };
        if code == 0 {
            Ok(Self { handle })
        } else {
            Err(code)
        }
    }

    fn open_for_change() -> Result<Self, SandboxError> {
        Self::open().map_err(|code| wfp_error("open the Windows Filtering Platform", code))
    }

    fn begin(&self) -> Result<Transaction<'_>, SandboxError> {
        // SAFETY: a valid engine handle; 0 asks for a read-write transaction.
        let code = unsafe { FwpmTransactionBegin0(self.handle, 0) };
        check(code, "begin a WFP transaction", &[])?;
        Ok(Transaction {
            engine: self,
            committed: false,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: the handle came from FwpmEngineOpen0 and is closed once.
        unsafe {
            FwpmEngineClose0(self.handle);
        }
    }
}

/// An open transaction, aborted on drop unless committed.
struct Transaction<'a> {
    engine: &'a Engine,
    committed: bool,
}

impl Transaction<'_> {
    fn commit(mut self) -> Result<(), SandboxError> {
        // SAFETY: a valid engine handle with an open transaction.
        let code = unsafe { FwpmTransactionCommit0(self.engine.handle) };
        check(code, "commit the WFP transaction", &[])?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: a valid engine handle with an open transaction.
            unsafe {
                FwpmTransactionAbort0(self.engine.handle);
            }
        }
    }
}

/// A self-relative security descriptor from SDDL, freed on drop.
struct Descriptor {
    sd: PSECURITY_DESCRIPTOR,
    size: u32,
}

impl Descriptor {
    fn from_sddl(sddl: &str) -> Result<Self, SandboxError> {
        let text = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let mut size = 0u32;
        // SAFETY: a NUL-terminated string, revision 1 (SDDL_REVISION_1), and
        // out pointers for the descriptor and its size.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut sd,
                &mut size,
            )
        };
        if ok == 0 {
            return Err(super::last_error(
                "build the DNS filter's user match descriptor",
            ));
        }
        Ok(Self { sd, size })
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            // SAFETY: allocated by ConvertStringSecurityDescriptor... with
            // LocalAlloc, freed once.
            unsafe {
                LocalFree(self.sd as HLOCAL);
            }
        }
    }
}

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB {
        size: 0,
        data: null_mut(),
    }
}

fn empty_value() -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_EMPTY,
        // SAFETY: a zeroed union is the empty value.
        Anonymous: unsafe { std::mem::zeroed() },
    }
}

/// Whether the object `key` of kind `object` exists. `Err` carries a raw
/// status other than "not found".
fn exists(engine: &Engine, object: WfpObject, key: u128) -> Result<bool, u32> {
    let k = guid(key);
    let mut out: *mut core::ffi::c_void = null_mut();
    // SAFETY: a valid engine handle, a key, and an out pointer that WFP
    // fills with memory freed below with FwpmFreeMemory0.
    let (code, missing) = unsafe {
        match object {
            WfpObject::Provider => (
                FwpmProviderGetByKey0(engine.handle, &k, (&raw mut out).cast()),
                FWP_E_PROVIDER_NOT_FOUND as u32,
            ),
            WfpObject::Sublayer => (
                FwpmSubLayerGetByKey0(engine.handle, &k, (&raw mut out).cast()),
                FWP_E_SUBLAYER_NOT_FOUND as u32,
            ),
            WfpObject::Filter => (
                FwpmFilterGetByKey0(engine.handle, &k, (&raw mut out).cast()),
                FWP_E_FILTER_NOT_FOUND as u32,
            ),
        }
    };
    if !out.is_null() {
        // SAFETY: memory WFP allocated for the call above.
        unsafe { FwpmFreeMemory0(&mut out) };
    }
    match code {
        0 => Ok(true),
        c if c == missing || c == FWP_E_NOT_FOUND as u32 => Ok(false),
        c => Err(c),
    }
}

/// The keys of `keys` that exist. `Err` says why they could not be read: a
/// standard user may be refused, which `kgraf sandbox status` shows as
/// unknown.
pub fn existing(keys: &WfpKeys) -> Result<BTreeSet<u128>, String> {
    let refused = |code: u32| {
        if code == E_ACCESSDENIED || code == ERROR_ACCESS_DENIED {
            "reading WFP filters needs administrator rights here".to_owned()
        } else {
            format!("the Windows Filtering Platform could not be read (0x{code:08X})")
        }
    };
    let engine = Engine::open().map_err(refused)?;
    let mut found = BTreeSet::new();
    for (object, key) in keys.all() {
        if exists(&engine, object, key).map_err(refused)? {
            found.insert(key);
        }
    }
    Ok(found)
}

/// Add kGraf's provider and sublayer where missing, and add the DNS filter
/// for the users `sids`, replacing any earlier copy. One transaction: a
/// failure leaves the machine as it was.
pub fn put(names: &OwnerNames, sids: &[&str]) -> Result<(), SandboxError> {
    let keys = WfpKeys::derive(names);
    let sddl = match_sddl(sids).map_err(|e| SandboxError::PlatformSetup {
        step: "build the DNS filter's user match descriptor",
        source: std::io::Error::other(e.to_string()),
    })?;
    let descriptor = Descriptor::from_sddl(&sddl)?;
    let engine = Engine::open_for_change()?;
    let txn = engine.begin()?;

    let mut provider_key = guid(keys.provider);
    let provider_name = wide(&display_name(names, WfpObject::Provider));
    let provider_text = wide(description(WfpObject::Provider));
    let provider = FWPM_PROVIDER0 {
        providerKey: provider_key,
        displayData: FWPM_DISPLAY_DATA0 {
            name: provider_name.as_ptr().cast_mut(),
            description: provider_text.as_ptr().cast_mut(),
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: empty_blob(),
        serviceName: null_mut(),
    };
    // SAFETY: a valid engine handle and a provider whose strings outlive the
    // call; no security descriptor (the default).
    let code = unsafe { FwpmProviderAdd0(engine.handle, &provider, null_mut()) };
    check(
        code,
        "add kGraf's WFP provider",
        &[FWP_E_ALREADY_EXISTS as u32],
    )?;

    let sublayer_name = wide(&display_name(names, WfpObject::Sublayer));
    let sublayer_text = wide(description(WfpObject::Sublayer));
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: guid(keys.sublayer),
        displayData: FWPM_DISPLAY_DATA0 {
            name: sublayer_name.as_ptr().cast_mut(),
            description: sublayer_text.as_ptr().cast_mut(),
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &raw mut provider_key,
        providerData: empty_blob(),
        weight: SUBLAYER_WEIGHT,
    };
    // SAFETY: as above.
    let code = unsafe { FwpmSubLayerAdd0(engine.handle, &sublayer, null_mut()) };
    check(
        code,
        "add kGraf's WFP sublayer",
        &[FWP_E_ALREADY_EXISTS as u32],
    )?;

    let filter_key = guid(keys.filter);
    // SAFETY: a valid engine handle and key.
    let code = unsafe { FwpmFilterDeleteByKey0(engine.handle, &filter_key) };
    check(
        code,
        "replace the DNS filter",
        &[FWP_E_FILTER_NOT_FOUND as u32, FWP_E_NOT_FOUND as u32],
    )?;

    let mut interface = FWP_BYTE_ARRAY16 {
        byteArray16: guid_bytes(DNS_RESOLVER_INTERFACE),
    };
    let mut token = FWP_BYTE_BLOB {
        size: descriptor.size,
        data: descriptor.sd.cast(),
    };
    let mut conditions = [
        FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_RPC_IF_UUID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_BYTE_ARRAY16_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    byteArray16: &raw mut interface,
                },
            },
        },
        FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_REMOTE_USER_TOKEN,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { sd: &raw mut token },
            },
        },
    ];
    let filter_name = wide(&display_name(names, WfpObject::Filter));
    let filter_text = wide(description(WfpObject::Filter));
    let filter = FWPM_FILTER0 {
        filterKey: filter_key,
        displayData: FWPM_DISPLAY_DATA0 {
            name: filter_name.as_ptr().cast_mut(),
            description: filter_text.as_ptr().cast_mut(),
        },
        // Persistent: it stays across restarts until `remove`. The action
        // right is cleared, so a permit in another sublayer cannot override
        // the block (only a veto callout can).
        flags: FWPM_FILTER_FLAG_PERSISTENT | FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT,
        providerKey: &raw mut provider_key,
        providerData: empty_blob(),
        layerKey: FWPM_LAYER_RPC_UM,
        subLayerKey: guid(keys.sublayer),
        weight: empty_value(),
        numFilterConditions: conditions.len() as u32,
        filterCondition: conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: FWP_ACTION_BLOCK,
            Anonymous: FWPM_ACTION0_0 {
                filterType: guid(0),
            },
        },
        Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
        reserved: null_mut(),
        filterId: 0,
        effectiveWeight: empty_value(),
    };
    let mut id = 0u64;
    // SAFETY: a valid engine handle; every pointer in `filter` points at a
    // local that lives until after the call.
    let code = unsafe { FwpmFilterAdd0(engine.handle, &filter, null_mut(), &mut id) };
    check(code, "add the DNS filter", &[])?;
    txn.commit()
}

/// Delete the object `key` of kind `object`. `Ok(false)` when it was already
/// gone.
pub fn remove(object: WfpObject, key: u128) -> Result<bool, SandboxError> {
    let engine = Engine::open_for_change()?;
    let k = guid(key);
    // SAFETY: a valid engine handle and key.
    let (code, missing, step) = unsafe {
        match object {
            WfpObject::Provider => (
                FwpmProviderDeleteByKey0(engine.handle, &k),
                FWP_E_PROVIDER_NOT_FOUND as u32,
                "remove kGraf's WFP provider",
            ),
            WfpObject::Sublayer => (
                FwpmSubLayerDeleteByKey0(engine.handle, &k),
                FWP_E_SUBLAYER_NOT_FOUND as u32,
                "remove kGraf's WFP sublayer",
            ),
            WfpObject::Filter => (
                FwpmFilterDeleteByKey0(engine.handle, &k),
                FWP_E_FILTER_NOT_FOUND as u32,
                "remove the DNS filter",
            ),
        }
    };
    if code == missing || code == FWP_E_NOT_FOUND as u32 {
        return Ok(false);
    }
    check(code, step, &[])?;
    Ok(true)
}
