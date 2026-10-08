//! Local users, the local group, denied logon rights and the sign-in list
//! entry, through NetAPI, LSA and the registry.
//!
//! Reading users and groups works for a standard user; changing them, and
//! reading or changing logon rights, needs administrator rights. Every call
//! names the step that failed.

use std::collections::BTreeSet;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MEMBER_IN_ALIAS, ERROR_NO_SUCH_ALIAS,
    ERROR_NONE_MAPPED, HLOCAL, LocalFree, STATUS_OBJECT_NAME_NOT_FOUND,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_1, LOCALGROUP_MEMBERS_INFO_3,
    LOCALGROUP_USERS_INFO_0, MAX_PREFERRED_LENGTH, NERR_GroupNotFound, NERR_Success,
    NERR_UserNotFound, NetApiBufferFree, NetLocalGroupAdd, NetLocalGroupAddMembers,
    NetLocalGroupDel, NetLocalGroupGetInfo, NetLocalGroupGetMembers, NetUserAdd, NetUserDel,
    NetUserGetInfo, NetUserGetLocalGroups, NetUserSetInfo, UF_DONT_EXPIRE_PASSWD,
    UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_INFO_1, USER_INFO_1003, USER_INFO_1007, USER_INFO_1008,
    USER_PRIV_USER,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose,
    LsaEnumerateAccountRights, LsaFreeMemory, LsaNtStatusToWinError, LsaOpenPolicy,
    LsaRemoveAccountRights, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
    RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};

use super::super::plan::UserFacts;
use super::{from_wide, last_error, wide, win32};
use crate::error::SandboxError;

// Upper-case names for NetAPI status codes used in `match` patterns.
const NERR_SUCCESS: u32 = NERR_Success;
const NERR_USER_NOT_FOUND: u32 = NERR_UserNotFound;
const NERR_GROUP_NOT_FOUND: u32 = NERR_GroupNotFound;

/// The well-known SID of the local Administrators group. Compared by SID,
/// because the group's name depends on the system language.
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// Winlogon's list of accounts kept off the sign-in screen: a DWORD value 0
/// named after the user hides it.
const USER_LIST: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

// ---- SIDs -------------------------------------------------------------------

/// The SID string of the local account `name`, or `None` when no account has
/// that name.
pub fn sid_of(name: &str) -> Result<Option<String>, SandboxError> {
    let account = wide(name);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut usage: SID_NAME_USE = 0;
    // SAFETY: a size query: null buffers with zero lengths; the call writes
    // only the two lengths and `usage`.
    let ok = unsafe {
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            ptr::null_mut(),
            &mut sid_len,
            ptr::null_mut(),
            &mut domain_len,
            &mut usage,
        )
    };
    if ok == 0 {
        // SAFETY: reads the thread's last error.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if code == ERROR_NONE_MAPPED {
            return Ok(None);
        }
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32("look up an account's SID", code));
        }
    }
    let mut sid = vec![0u8; sid_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    // SAFETY: both buffers have the lengths the size query returned.
    let ok = unsafe {
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut usage,
        )
    };
    if ok == 0 {
        return Err(last_error("look up an account's SID"));
    }
    sid_to_string(sid.as_mut_ptr().cast()).map(Some)
}

fn sid_to_string(sid: PSID) -> Result<String, SandboxError> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID; on success the call allocates `text`,
    // which is freed below with LocalFree.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last_error("write a SID as text"));
    }
    // SAFETY: `text` is the NUL-terminated string the call allocated.
    let s = unsafe { from_wide(text) };
    // SAFETY: frees the buffer ConvertSidToStringSidW allocated.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(s)
}

/// A SID parsed from its text form, freed on drop.
struct OwnedSid(PSID);

impl OwnedSid {
    fn parse(text: &str) -> Result<Self, SandboxError> {
        let w = wide(text);
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: `w` is NUL-terminated; on success `sid` is allocated by the
        // call and freed in `Drop`.
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
            return Err(last_error("read a SID from text"));
        }
        Ok(Self(sid))
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        // SAFETY: frees the SID ConvertStringSidToSidW allocated.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

// ---- users ------------------------------------------------------------------

/// The facts of local user `name`, or `None` when it does not exist.
pub fn user_facts(name: &str) -> Result<Option<UserFacts>, SandboxError> {
    let user = wide(name);
    let mut buf: *mut u8 = ptr::null_mut();
    // SAFETY: level 1 fills a USER_INFO_1 the call allocates; it is freed with
    // NetApiBufferFree below.
    let rc = unsafe { NetUserGetInfo(ptr::null(), user.as_ptr(), 1, &mut buf) };
    if rc == NERR_UserNotFound {
        return Ok(None);
    }
    if rc != NERR_Success {
        return Err(win32("read a local user", rc));
    }
    // SAFETY: on success `buf` points to one USER_INFO_1.
    let comment = unsafe { from_wide((*buf.cast::<USER_INFO_1>()).usri1_comment) };
    // SAFETY: frees the buffer NetUserGetInfo allocated.
    unsafe { NetApiBufferFree(buf.cast()) };
    let sid = sid_of(name)?.ok_or_else(|| win32("look up an account's SID", ERROR_NONE_MAPPED))?;
    let in_administrators = local_groups_of(name)?
        .iter()
        .map(|g| sid_of(g))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|s| s.as_deref() == Some(ADMINISTRATORS_SID));
    Ok(Some(UserFacts {
        sid,
        comment,
        in_administrators,
    }))
}

fn local_groups_of(name: &str) -> Result<Vec<String>, SandboxError> {
    let user = wide(name);
    let mut buf: *mut u8 = ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: level 0 fills an array of LOCALGROUP_USERS_INFO_0 the call
    // allocates; freed below.
    let rc = unsafe {
        NetUserGetLocalGroups(
            ptr::null(),
            user.as_ptr(),
            0,
            0,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
        )
    };
    if rc != NERR_Success {
        return Err(win32("list a user's local groups", rc));
    }
    let mut groups = Vec::with_capacity(read as usize);
    for i in 0..read as usize {
        // SAFETY: `buf` holds `read` entries.
        let entry = unsafe { &*buf.cast::<LOCALGROUP_USERS_INFO_0>().add(i) };
        // SAFETY: each name is a NUL-terminated string inside the buffer.
        groups.push(unsafe { from_wide(entry.lgrui0_name) });
    }
    if !buf.is_null() {
        // SAFETY: frees the buffer the call allocated.
        unsafe { NetApiBufferFree(buf.cast()) };
    }
    Ok(groups)
}

const USER_FLAGS: u32 = UF_SCRIPT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE;

/// Create a standard local user with a password that does not expire.
pub fn create_user(name: &str, password: &str, comment: &str) -> Result<(), SandboxError> {
    let mut n = wide(name);
    let mut p = wide(password);
    let mut c = wide(comment);
    let info = USER_INFO_1 {
        usri1_name: n.as_mut_ptr(),
        usri1_password: p.as_mut_ptr(),
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: ptr::null_mut(),
        usri1_comment: c.as_mut_ptr(),
        usri1_flags: USER_FLAGS,
        usri1_script_path: ptr::null_mut(),
    };
    let mut parm_err = 0u32;
    // SAFETY: `info` points into buffers that live across the call.
    let rc = unsafe { NetUserAdd(ptr::null(), 1, (&raw const info).cast(), &mut parm_err) };
    p.iter_mut().for_each(|u| *u = 0);
    if rc != NERR_Success {
        return Err(win32("create a local user", rc));
    }
    Ok(())
}

/// Reset an existing user's password, comment and flags.
pub fn update_user(name: &str, password: &str, comment: &str) -> Result<(), SandboxError> {
    let user = wide(name);
    let mut p = wide(password);
    let mut c = wide(comment);
    let mut parm_err = 0u32;
    let pw = USER_INFO_1003 {
        usri1003_password: p.as_mut_ptr(),
    };
    // SAFETY: `pw` points into `p`, which lives across the call.
    let rc = unsafe {
        NetUserSetInfo(
            ptr::null(),
            user.as_ptr(),
            1003,
            (&raw const pw).cast(),
            &mut parm_err,
        )
    };
    p.iter_mut().for_each(|u| *u = 0);
    if rc != NERR_Success {
        return Err(win32("reset a local user's password", rc));
    }
    let cm = USER_INFO_1007 {
        usri1007_comment: c.as_mut_ptr(),
    };
    // SAFETY: `cm` points into `c`, which lives across the call.
    let rc = unsafe {
        NetUserSetInfo(
            ptr::null(),
            user.as_ptr(),
            1007,
            (&raw const cm).cast(),
            &mut parm_err,
        )
    };
    if rc != NERR_Success {
        return Err(win32("set a local user's comment", rc));
    }
    let fl = USER_INFO_1008 {
        usri1008_flags: USER_FLAGS,
    };
    // SAFETY: `fl` is a plain value that lives across the call.
    let rc = unsafe {
        NetUserSetInfo(
            ptr::null(),
            user.as_ptr(),
            1008,
            (&raw const fl).cast(),
            &mut parm_err,
        )
    };
    if rc != NERR_Success {
        return Err(win32("set a local user's flags", rc));
    }
    Ok(())
}

/// Delete a local user. `false` when it did not exist.
pub fn delete_user(name: &str) -> Result<bool, SandboxError> {
    let user = wide(name);
    // SAFETY: `user` is NUL-terminated.
    match unsafe { NetUserDel(ptr::null(), user.as_ptr()) } {
        NERR_SUCCESS => Ok(true),
        NERR_USER_NOT_FOUND => Ok(false),
        rc => Err(win32("delete a local user", rc)),
    }
}

// ---- the group --------------------------------------------------------------

/// The comment of local group `name`, or `None` when it does not exist.
pub fn group_comment(name: &str) -> Result<Option<String>, SandboxError> {
    let group = wide(name);
    let mut buf: *mut u8 = ptr::null_mut();
    // SAFETY: level 1 fills a LOCALGROUP_INFO_1 the call allocates; freed
    // below.
    let rc = unsafe { NetLocalGroupGetInfo(ptr::null(), group.as_ptr(), 1, &mut buf) };
    if rc == NERR_GroupNotFound || rc == ERROR_NO_SUCH_ALIAS {
        return Ok(None);
    }
    if rc != NERR_Success {
        return Err(win32("read a local group", rc));
    }
    // SAFETY: on success `buf` points to one LOCALGROUP_INFO_1.
    let comment = unsafe { from_wide((*buf.cast::<LOCALGROUP_INFO_1>()).lgrpi1_comment) };
    // SAFETY: frees the buffer the call allocated.
    unsafe { NetApiBufferFree(buf.cast()) };
    Ok(Some(comment))
}

/// Create local group `name`.
pub fn create_group(name: &str, comment: &str) -> Result<(), SandboxError> {
    let mut n = wide(name);
    let mut c = wide(comment);
    let info = LOCALGROUP_INFO_1 {
        lgrpi1_name: n.as_mut_ptr(),
        lgrpi1_comment: c.as_mut_ptr(),
    };
    let mut parm_err = 0u32;
    // SAFETY: `info` points into buffers that live across the call.
    let rc = unsafe { NetLocalGroupAdd(ptr::null(), 1, (&raw const info).cast(), &mut parm_err) };
    if rc != NERR_Success {
        return Err(win32("create a local group", rc));
    }
    Ok(())
}

/// The account names in local group `name`.
pub fn group_members(name: &str) -> Result<BTreeSet<String>, SandboxError> {
    let group = wide(name);
    let mut buf: *mut u8 = ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    let mut resume = 0usize;
    // SAFETY: level 1 fills an array of LOCALGROUP_MEMBERS_INFO_1 the call
    // allocates; freed below.
    let rc = unsafe {
        NetLocalGroupGetMembers(
            ptr::null(),
            group.as_ptr(),
            1,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
            &mut resume,
        )
    };
    if rc == NERR_GroupNotFound || rc == ERROR_NO_SUCH_ALIAS {
        return Ok(BTreeSet::new());
    }
    if rc != NERR_Success {
        return Err(win32("list a local group's members", rc));
    }
    let mut members = BTreeSet::new();
    for i in 0..read as usize {
        // SAFETY: `buf` holds `read` entries.
        let entry = unsafe { &*buf.cast::<LOCALGROUP_MEMBERS_INFO_1>().add(i) };
        // SAFETY: each name is a NUL-terminated string inside the buffer.
        members.insert(unsafe { from_wide(entry.lgrmi1_name) });
    }
    if !buf.is_null() {
        // SAFETY: frees the buffer the call allocated.
        unsafe { NetApiBufferFree(buf.cast()) };
    }
    Ok(members)
}

/// Add local user `user` to local group `group`; a member already in it is
/// fine.
pub fn add_member(group: &str, user: &str) -> Result<(), SandboxError> {
    let g = wide(group);
    let mut u = wide(user);
    let entry = LOCALGROUP_MEMBERS_INFO_3 {
        lgrmi3_domainandname: u.as_mut_ptr(),
    };
    // SAFETY: `entry` points into `u`, which lives across the call.
    let rc = unsafe {
        NetLocalGroupAddMembers(ptr::null(), g.as_ptr(), 3, (&raw const entry).cast(), 1)
    };
    if rc != NERR_Success && rc != ERROR_MEMBER_IN_ALIAS {
        return Err(win32("add a user to a local group", rc));
    }
    Ok(())
}

/// Delete local group `name`. `false` when it did not exist.
pub fn delete_group(name: &str) -> Result<bool, SandboxError> {
    let group = wide(name);
    // SAFETY: `group` is NUL-terminated.
    match unsafe { NetLocalGroupDel(ptr::null(), group.as_ptr()) } {
        NERR_SUCCESS => Ok(true),
        NERR_GROUP_NOT_FOUND | ERROR_NO_SUCH_ALIAS => Ok(false),
        rc => Err(win32("delete a local group", rc)),
    }
}

// ---- logon rights -----------------------------------------------------------

/// The local security policy, opened for account rights; closed on drop.
struct Policy(LSA_HANDLE);

impl Policy {
    fn open() -> Result<Self, SandboxError> {
        // SAFETY: LSA_OBJECT_ATTRIBUTES is plain data; zeroed is the
        // documented "no attributes" value.
        let attrs: LSA_OBJECT_ATTRIBUTES = unsafe { std::mem::zeroed() };
        let mut handle: LSA_HANDLE = 0;
        // SAFETY: a null system name opens the local policy; `handle` is
        // written on success and closed in `Drop`.
        let status = unsafe {
            LsaOpenPolicy(
                ptr::null(),
                &attrs,
                (POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT) as u32,
                &mut handle,
            )
        };
        if status != 0 {
            return Err(lsa_error("open the local security policy", status));
        }
        Ok(Self(handle))
    }
}

impl Drop for Policy {
    fn drop(&mut self) {
        // SAFETY: closes the handle LsaOpenPolicy returned.
        unsafe { LsaClose(self.0) };
    }
}

fn lsa_error(step: &'static str, status: i32) -> SandboxError {
    // SAFETY: a pure status translation.
    win32(step, unsafe { LsaNtStatusToWinError(status) })
}

/// The LSA string for NUL-terminated `w`, borrowing it.
fn lsa_string(w: &mut [u16]) -> LSA_UNICODE_STRING {
    let chars = w.len().saturating_sub(1);
    LSA_UNICODE_STRING {
        Length: (chars * 2) as u16,
        MaximumLength: (w.len() * 2) as u16,
        Buffer: w.as_mut_ptr(),
    }
}

/// The account rights the account with SID `sid` holds directly.
pub fn rights_of(sid: &str) -> Result<BTreeSet<String>, SandboxError> {
    let policy = Policy::open()?;
    let sid = OwnedSid::parse(sid)?;
    let mut rights: *mut LSA_UNICODE_STRING = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: on success the call allocates `rights` (freed with
    // LsaFreeMemory below) holding `count` strings.
    let status = unsafe { LsaEnumerateAccountRights(policy.0, sid.0, &mut rights, &mut count) };
    if status == STATUS_OBJECT_NAME_NOT_FOUND {
        return Ok(BTreeSet::new());
    }
    if status != 0 {
        return Err(lsa_error("read an account's logon rights", status));
    }
    let mut out = BTreeSet::new();
    for i in 0..count as usize {
        // SAFETY: `rights` holds `count` entries; each Buffer holds Length
        // bytes of UTF-16, not NUL-terminated.
        let s = unsafe { &*rights.add(i) };
        let units = unsafe { std::slice::from_raw_parts(s.Buffer, (s.Length / 2) as usize) };
        out.insert(String::from_utf16_lossy(units));
    }
    // SAFETY: frees the buffer LsaEnumerateAccountRights allocated.
    unsafe { LsaFreeMemory(rights.cast()) };
    Ok(out)
}

fn change_rights(sid: &str, rights: &[&str], add: bool) -> Result<(), SandboxError> {
    if rights.is_empty() {
        return Ok(());
    }
    let policy = Policy::open()?;
    let sid = OwnedSid::parse(sid)?;
    let mut wides: Vec<Vec<u16>> = rights.iter().map(|r| wide(r)).collect();
    let strings: Vec<LSA_UNICODE_STRING> = wides.iter_mut().map(|w| lsa_string(w)).collect();
    // SAFETY: `strings` borrows `wides`, both alive across the call.
    let status = unsafe {
        if add {
            LsaAddAccountRights(policy.0, sid.0, strings.as_ptr(), strings.len() as u32)
        } else {
            LsaRemoveAccountRights(
                policy.0,
                sid.0,
                false,
                strings.as_ptr(),
                strings.len() as u32,
            )
        }
    };
    if status == STATUS_OBJECT_NAME_NOT_FOUND && !add {
        return Ok(());
    }
    if status != 0 {
        return Err(lsa_error(
            if add {
                "grant an account a logon right"
            } else {
                "remove an account's logon right"
            },
            status,
        ));
    }
    Ok(())
}

/// Give the account with SID `sid` each right in `rights`.
pub fn add_rights(sid: &str, rights: &[&str]) -> Result<(), SandboxError> {
    change_rights(sid, rights, true)
}

/// Take each right in `rights` from the account with SID `sid`.
pub fn remove_rights(sid: &str, rights: &[&str]) -> Result<(), SandboxError> {
    change_rights(sid, rights, false)
}

// ---- the sign-in list -------------------------------------------------------

/// Whether `user` is kept off the sign-in screen.
pub fn is_hidden(user: &str) -> Result<bool, SandboxError> {
    let key = wide(USER_LIST);
    let value = wide(user);
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: reads one DWORD into `data` from the 64-bit registry view.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
            ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    match rc {
        0 => Ok(data == 0),
        ERROR_FILE_NOT_FOUND => Ok(false),
        rc => Err(win32("read the sign-in list", rc)),
    }
}

/// Keep `user` off the sign-in screen.
pub fn hide(user: &str) -> Result<(), SandboxError> {
    let key = wide(USER_LIST);
    let value = wide(user);
    let mut handle: HKEY = ptr::null_mut();
    // SAFETY: creates or opens the key for writing; closed below.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            ptr::null(),
            &mut handle,
            ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(win32("open the sign-in list", rc));
    }
    let zero = 0u32.to_le_bytes();
    // SAFETY: writes a 4-byte DWORD value under the open key.
    let rc = unsafe {
        RegSetValueExW(
            handle,
            value.as_ptr(),
            0,
            REG_DWORD,
            zero.as_ptr(),
            zero.len() as u32,
        )
    };
    // SAFETY: closes the key opened above.
    unsafe { RegCloseKey(handle) };
    if rc != 0 {
        return Err(win32("hide a user from the sign-in screen", rc));
    }
    Ok(())
}

/// Put `user` back on the sign-in screen (removes its value).
pub fn unhide(user: &str) -> Result<(), SandboxError> {
    let key = wide(USER_LIST);
    let value = wide(user);
    let mut handle: HKEY = ptr::null_mut();
    // SAFETY: opens the key for writing; closed below.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            &mut handle,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if rc != 0 {
        return Err(win32("open the sign-in list", rc));
    }
    // SAFETY: deletes one value under the open key.
    let rc = unsafe { RegDeleteValueW(handle, value.as_ptr()) };
    // SAFETY: closes the key opened above.
    unsafe { RegCloseKey(handle) };
    if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
        return Err(win32("remove a user from the sign-in list", rc));
    }
    Ok(())
}

// ---- the current user and logon checks --------------------------------------

/// The SID of the user this process runs as.
pub fn current_user_sid() -> Result<String, SandboxError> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: opens this process's own token for query; closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("open this process's token"));
    }
    let mut len = 0u32;
    // SAFETY: a size query with a null buffer.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes, 8-byte aligned for TOKEN_USER.
    let ok =
        unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) };
    // SAFETY: closes the token opened above.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(last_error("read this process's user"));
    }
    // SAFETY: on success `buf` begins with a TOKEN_USER whose SID points
    // into `buf`.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    sid_to_string(sid)
}

/// Whether `user` can log on with `password` (an interactive logon, the kind
/// the secondary logon service performs). The token is closed at once.
pub fn logon_works(user: &str, password: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, LogonUserW,
    };
    let u = wide(user);
    let dot = wide(".");
    let mut p = wide(password);
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the strings live across the call; a token is returned on
    // success and closed below.
    let ok = unsafe {
        LogonUserW(
            u.as_ptr(),
            dot.as_ptr(),
            p.as_ptr(),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    let err = std::io::Error::last_os_error();
    p.iter_mut().for_each(|c| *c = 0);
    if ok == 0 {
        return Err(err.to_string());
    }
    // SAFETY: closes the token LogonUserW returned.
    unsafe { CloseHandle(token) };
    Ok(())
}
