//! Name lookups through the DNS Client service, for the DNS check of
//! `kgraf sandbox selfcheck` and the Windows tests
//! ([`super::super::dns_check`] judges the results).

use std::ptr::null_mut;

use windows_sys::Win32::NetworkManagement::Dns::{
    DNS_QUERY_CACHE_ONLY, DNS_QUERY_STANDARD, DNS_RECORDA, DNS_TYPE_A, DnsFree, DnsFreeRecordList,
    DnsQuery_W,
};

use super::wide;

/// Look `name` up (type A) through the DNS Client service and return the
/// status: 0 for an answer, a `DNS_ERROR_*` or Win32 code otherwise. With
/// `cache_only` the service answers from its cache and sends nothing.
pub fn query(name: &str, cache_only: bool) -> u32 {
    let text = wide(name);
    let options = if cache_only {
        DNS_QUERY_CACHE_ONLY
    } else {
        DNS_QUERY_STANDARD
    };
    let mut records: *mut DNS_RECORDA = null_mut();
    // SAFETY: a NUL-terminated name, no extra servers, and an out pointer
    // whose list is freed below.
    let code = unsafe {
        DnsQuery_W(
            text.as_ptr(),
            DNS_TYPE_A,
            options,
            null_mut(),
            &mut records,
            null_mut(),
        )
    };
    if !records.is_null() {
        // SAFETY: the list DnsQuery_W returned, freed once.
        unsafe { DnsFree(records.cast(), DnsFreeRecordList) };
    }
    code
}
