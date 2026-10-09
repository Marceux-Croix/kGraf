//! The DNS check of `kgraf sandbox selfcheck` on Windows: does a name lookup
//! by a blocked sandbox user leave the machine?
//!
//! The check proves the DNS filter by its effect, never by its presence. It
//! uses three names nobody has looked up before, each
//! `kgraf-probe-<32 hex>.invalid`:
//!
//! 1. **Control.** The owner's own process looks up name A. A DNS server's
//!    answer (here a name error, because `.invalid` does not exist) proves
//!    that this machine can reach a server, so a failed lookup in the sandbox
//!    means something.
//! 2. **Probe.** The sandbox user looks up name B. A server's answer means the
//!    lookup left the machine.
//! 3. **Cache.** The owner asks the DNS Client service's cache, and sends
//!    nothing, about A (looked up), C (never looked up) and B. The service
//!    keeps a name error in its cache, so B must look like C, not like A. If
//!    A and C look alike, the cache cannot tell, and nothing is proved.
//!
//! A blocked RPC call and a server's name error can look alike to the
//! caller, so the probe's own status is not enough; the cache is a second,
//! independent witness. The verdict is a pass only when every step proves
//! confinement. Any doubt is a failure, never a pass.
//!
//! The probe runs `Resolve-DnsName` in Windows PowerShell, which asks the DNS
//! Client service like any program does, and prints the status code.

use std::fmt;

/// A fresh name under the reserved `.invalid` top-level domain, which no
/// server answers with an address. 16 random bytes keep it from any cache.
pub fn probe_name() -> Result<String, getrandom::Error> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b)?;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("kgraf-probe-{hex}.invalid"))
}

/// Whether `s` is a name [`probe_name`] makes, so it can go into a script
/// without quoting.
pub fn is_probe_name(s: &str) -> bool {
    s.strip_prefix("kgraf-probe-")
        .and_then(|r| r.strip_suffix(".invalid"))
        .is_some_and(|hex| {
            hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// The marker before the status code in the probe's output.
pub const CODE_MARK: &str = "KGRAF-DNS-CODE=";
/// The marker before the error's identifier and message.
pub const ERROR_MARK: &str = "KGRAF-DNS-ERROR=";

/// The PowerShell script that looks `name` up and prints [`CODE_MARK`] with
/// the status (0 for an answer), then [`ERROR_MARK`] with the error's
/// identifier and message. `-DnsOnly` leaves out LLMNR and NetBIOS, which
/// send on the local network; `-QuickTimeout` keeps a blocked lookup short.
///
/// Returns `None` for a name [`probe_name`] did not make.
pub fn probe_script(name: &str) -> Option<String> {
    if !is_probe_name(name) {
        return None;
    }
    Some(format!(
        "try {{ Resolve-DnsName -Name '{name}' -Type A -DnsOnly -QuickTimeout -ErrorAction Stop \
         | Out-Null; '{CODE_MARK}0' }} catch {{ $e = $_.Exception; $c = $null; \
         while ($e) {{ if ($e -is [System.ComponentModel.Win32Exception]) \
         {{ $c = $e.NativeErrorCode; break }}; $e = $e.InnerException }}; \
         if ($null -eq $c) {{ '{CODE_MARK}?' }} else {{ '{CODE_MARK}' + $c }}; \
         '{ERROR_MARK}' + $_.FullyQualifiedErrorId + ' ' + $_.Exception.Message }}"
    ))
}

/// What the probe printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// The status code, when the probe could tell it.
    pub code: Option<u32>,
    /// The error's identifier and message (empty for an answer).
    pub error: String,
}

/// The probe's result from its output, or `None` when it printed no status
/// (it did not run, or PowerShell failed before the lookup).
pub fn parse_probe(stdout: &str) -> Option<ProbeResult> {
    let mut code = None;
    let mut seen = false;
    let mut error = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(c) = line.strip_prefix(CODE_MARK) {
            seen = true;
            // A negative Int32 from PowerShell is an HRESULT-style code.
            code = c
                .parse::<u32>()
                .ok()
                .or_else(|| c.parse::<i32>().ok().map(|v| v as u32));
        } else if let Some(e) = line.strip_prefix(ERROR_MARK) {
            error = e.to_owned();
        }
    }
    seen.then_some(ProbeResult { code, error })
}

/// `DNS_ERROR_RCODE_NAME_ERROR`: a server said the name does not exist.
pub const NAME_ERROR: u32 = 9003;
/// `DNS_INFO_NO_RECORDS`: a server said the name has no record of the type.
pub const NO_RECORDS: u32 = 9501;

/// Whether `code` is a DNS server's answer: an address (0), one of the
/// `DNS_ERROR_RCODE_*` codes (9001 to 9018, the server's own response code),
/// or [`NO_RECORDS`].
pub fn is_server_answer(code: u32) -> bool {
    code == 0 || (9001..=9018).contains(&code) || code == NO_RECORDS
}

/// Whether the probe got a server's answer, by its code or, where it printed
/// none, by the identifier of its error.
pub fn probe_reached_a_server(p: &ProbeResult) -> bool {
    match p.code {
        Some(c) => is_server_answer(c),
        None => {
            let id = p.error.split([',', ' ']).next().unwrap_or_default();
            id.starts_with("DNS_ERROR_RCODE_") || id == "DNS_INFO_NO_RECORDS"
        }
    }
}

/// Everything the check gathered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsEvidence {
    /// The owner's own lookup of name A.
    pub control: u32,
    /// The sandbox user's lookup of name B, or `None` when the probe printed
    /// no status, with what it printed.
    pub probe: Result<ProbeResult, String>,
    /// The cache-only status of A, which was looked up.
    pub cache_known: u32,
    /// The cache-only status of C, which nobody looked up.
    pub cache_fresh: u32,
    /// The cache-only status of B.
    pub cache_probe: u32,
}

/// The check's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsVerdict {
    /// The lookup did not reach a server and the service did not look it up.
    Confined,
    /// The lookup left the machine.
    Leaked(String),
    /// Nothing could be proved either way. Not a pass.
    NotProved(String),
}

impl DnsVerdict {
    pub fn passed(&self) -> bool {
        matches!(self, DnsVerdict::Confined)
    }
}

impl fmt::Display for DnsVerdict {
    /// The line `kgraf sandbox selfcheck` prints, without its indent.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DnsVerdict::Confined => write!(
                f,
                "dns: confined (the sandbox user's lookup reached no DNS server, and the DNS \
                 Client service did not look the name up)"
            ),
            DnsVerdict::Leaked(why) => {
                write!(f, "dns: not confined (lookups leave the machine): {why}")
            }
            DnsVerdict::NotProved(why) => write!(f, "dns: not proved: {why}"),
        }
    }
}

/// The verdict from `e`. See the module header for the reasoning.
pub fn judge(e: &DnsEvidence) -> DnsVerdict {
    let probe = match &e.probe {
        Ok(p) => p,
        Err(output) => {
            return DnsVerdict::NotProved(format!(
                "the lookup in the sandbox printed no status: {output}"
            ));
        }
    };
    if probe_reached_a_server(probe) {
        return DnsVerdict::Leaked(format!(
            "the sandbox user's lookup got a DNS server's answer ({})",
            describe(probe)
        ));
    }
    if e.cache_probe == e.cache_known && e.cache_known != e.cache_fresh {
        return DnsVerdict::Leaked(format!(
            "the DNS Client service cached the sandbox user's name (cache status {})",
            e.cache_probe
        ));
    }
    if !is_server_answer(e.control) {
        return DnsVerdict::NotProved(format!(
            "this machine's own lookup reached no DNS server (status {}), so a failed lookup in \
             the sandbox proves nothing; run the check with a working network",
            e.control
        ));
    }
    if e.cache_known == e.cache_fresh {
        return DnsVerdict::NotProved(format!(
            "the DNS Client cache answers a looked-up name and a new one alike (status {}), so \
             it cannot show whether the service looked the sandbox user's name up",
            e.cache_known
        ));
    }
    if e.cache_probe != e.cache_fresh {
        return DnsVerdict::NotProved(format!(
            "the cache status of the sandbox user's name ({}) matches neither a looked-up name \
             ({}) nor a new one ({})",
            e.cache_probe, e.cache_known, e.cache_fresh
        ));
    }
    DnsVerdict::Confined
}

fn describe(p: &ProbeResult) -> String {
    let code = p.code.map_or_else(|| "?".to_owned(), |c| c.to_string());
    if p.error.is_empty() {
        format!("status {code}")
    } else {
        format!("status {code}, {}", p.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a confined run looks like: the owner reached a server, the probe
    /// failed without one (here an RPC access refusal), and the cache tells
    /// the names apart and does not hold the probe's.
    fn confined() -> DnsEvidence {
        DnsEvidence {
            control: NAME_ERROR,
            probe: Ok(ProbeResult {
                code: Some(5),
                error: "ERROR_ACCESS_DENIED,Microsoft.DnsClient.Commands.ResolveDnsName".into(),
            }),
            cache_known: NAME_ERROR,
            cache_fresh: 9701,
            cache_probe: 9701,
        }
    }

    #[test]
    fn probe_names_are_fresh_and_safe_to_embed() {
        let a = probe_name().unwrap();
        let b = probe_name().unwrap();
        assert_ne!(a, b);
        assert!(is_probe_name(&a), "{a}");
        // One DNS label of at most 63 characters before the top level.
        assert!(a.split('.').next().unwrap().len() <= 63);
        for bad in [
            "example.com",
            "kgraf-probe-x.invalid",
            "kgraf-probe-0123456789abcdef0123456789abcdef.invalid'; rm",
            "kgraf-probe-0123456789ABCDEF0123456789abcdef.invalid",
        ] {
            assert!(!is_probe_name(bad), "{bad}");
            assert_eq!(probe_script(bad), None);
        }
        let script = probe_script(&a).unwrap();
        assert!(script.contains(&format!("'{a}'")));
        assert!(script.contains("-DnsOnly"));
        assert!(script.contains(CODE_MARK) && script.contains(ERROR_MARK));
    }

    #[test]
    fn probe_output_parses_with_and_without_a_code() {
        let out = format!(
            "noise\r\n{CODE_MARK}9003\r\n{ERROR_MARK}DNS_ERROR_RCODE_NAME_ERROR,Microsoft.DnsClient.Commands.ResolveDnsName DNS name does not exist\r\n"
        );
        let p = parse_probe(&out).unwrap();
        assert_eq!(p.code, Some(NAME_ERROR));
        assert!(probe_reached_a_server(&p));
        let p = parse_probe(&format!(
            "{CODE_MARK}?\n{ERROR_MARK}DNS_ERROR_RCODE_NAME_ERROR,x y"
        ))
        .unwrap();
        assert_eq!(p.code, None);
        assert!(probe_reached_a_server(&p), "judged by the identifier");
        let p = parse_probe(&format!("{CODE_MARK}-2147024891")).unwrap();
        assert_eq!(p.code, Some(0x8007_0005));
        assert!(!probe_reached_a_server(&p));
        assert_eq!(parse_probe("powershell: not recognized"), None);
        assert_eq!(parse_probe(&format!("{CODE_MARK}0")).unwrap().code, Some(0));
    }

    #[test]
    fn server_answers_are_told_from_local_failures() {
        for c in [0, 9001, 9002, NAME_ERROR, 9005, 9018, NO_RECORDS] {
            assert!(is_server_answer(c), "{c}");
        }
        // Access denied, RPC failures, timeout, no servers, socket refused.
        for c in [5, 1460, 1722, 1753, 9852, 10013, 11001, 0x8007_0005] {
            assert!(!is_server_answer(c), "{c}");
        }
    }

    #[test]
    fn only_a_fully_proved_run_passes() {
        assert_eq!(judge(&confined()), DnsVerdict::Confined);
        assert!(judge(&confined()).passed());
        assert!(judge(&confined()).to_string().starts_with("dns: confined"));
    }

    #[test]
    fn a_server_answer_in_the_sandbox_is_a_leak_whatever_the_cache_says() {
        for code in [0, NAME_ERROR, NO_RECORDS] {
            let mut e = confined();
            e.probe = Ok(ProbeResult {
                code: Some(code),
                error: String::new(),
            });
            let v = judge(&e);
            assert!(matches!(v, DnsVerdict::Leaked(_)), "{code}: {v:?}");
            assert!(
                v.to_string()
                    .starts_with("dns: not confined (lookups leave the machine)")
            );
        }
    }

    #[test]
    fn a_cached_probe_name_is_a_leak_even_when_the_probe_failed() {
        let mut e = confined();
        e.cache_probe = e.cache_known;
        assert!(matches!(judge(&e), DnsVerdict::Leaked(_)));
    }

    #[test]
    fn doubt_is_never_a_pass() {
        // No network for the owner either: a failed lookup proves nothing.
        let mut e = confined();
        e.control = 1460;
        assert!(matches!(judge(&e), DnsVerdict::NotProved(_)));
        // A cache that cannot tell the names apart.
        let mut e = confined();
        e.cache_known = 9701;
        assert!(matches!(judge(&e), DnsVerdict::NotProved(_)));
        // A cache answer that fits neither.
        let mut e = confined();
        e.cache_probe = 1460;
        assert!(matches!(judge(&e), DnsVerdict::NotProved(_)));
        // The probe printed nothing.
        let mut e = confined();
        e.probe = Err("the term 'Resolve-DnsName' is not recognized".into());
        let v = judge(&e);
        assert!(matches!(v, DnsVerdict::NotProved(_)));
        assert!(!v.passed());
        assert!(v.to_string().starts_with("dns: not proved"));
    }
}
