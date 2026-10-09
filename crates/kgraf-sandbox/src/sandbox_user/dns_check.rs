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
//! A blocked lookup and a server's name error can look alike to the
//! caller, so the probe's own status is not enough; the cache is a second,
//! independent witness. The verdict is a pass only when every step proves
//! confinement. Any doubt is a failure, never a pass.
//!
//! **The probe must prove a local failure.** The probe's status is sorted
//! three ways ([`classify_probe`]): a DNS server's answer is a leak, a named
//! local refusal ([`LOCAL_REFUSALS`]) is the confined side, and anything else,
//! a timeout included, is `not proved` with the status named. There is no "not
//! a server answer, so a local failure" case: a query that left the machine
//! and got no answer in time also fails inside the process. The probe prints
//! [`END_MARK`] last, and output without it is refused, so a tail cut off
//! after a normal exit is seen for what it is instead of read as a pass.
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
/// The last line the probe prints, whatever the lookup did. Output without
/// it was cut short, so it is not evidence: after a normal exit the readers
/// get only a short grace, and a truncated tail is returned as if it were
/// whole.
pub const END_MARK: &str = "KGRAF-DNS-END";

/// The PowerShell script that looks `name` up and prints [`CODE_MARK`] with
/// the status (0 for an answer), then [`ERROR_MARK`] with the error's
/// identifier and message, then [`END_MARK`]. `-DnsOnly` leaves out LLMNR
/// and NetBIOS, which send on the local network; `-QuickTimeout` keeps a
/// blocked lookup short.
///
/// [`END_MARK`] is printed after the `try` and the `catch`, so it is the last
/// line of either path.
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
         '{ERROR_MARK}' + $_.FullyQualifiedErrorId + ' ' + $_.Exception.Message }}; \
         '{END_MARK}'"
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

/// The probe's result from its output, or `None` when the output is not
/// evidence: no [`END_MARK`], so it was cut short or the probe never ran, or
/// an end but no witness at all (neither a status code nor a named error).
///
/// A missing witness must never read as a pass, so the caller turns `None`
/// into [`DnsVerdict::NotProved`]. The line `KGRAF-DNS-CODE=` with nothing
/// after it used to set the "a status was printed" flag by its prefix alone
/// and leave the code unset, which the old polarity read as a local failure,
/// that is, as confinement.
pub fn parse_probe(stdout: &str) -> Option<ProbeResult> {
    let mut code = None;
    let mut ended = false;
    let mut error = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(c) = line.strip_prefix(CODE_MARK) {
            // A negative Int32 from PowerShell is an HRESULT-style code.
            code = c
                .parse::<u32>()
                .ok()
                .or_else(|| c.parse::<i32>().ok().map(|v| v as u32));
        } else if let Some(e) = line.strip_prefix(ERROR_MARK) {
            error = e.to_owned();
        } else if line == END_MARK {
            ended = true;
        }
    }
    let witness = code.is_some() || !error.is_empty();
    (ended && witness).then_some(ProbeResult { code, error })
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

/// The local failures that name themselves: each one says that the lookup
/// never reached a DNS server, so each is the confined side. Anything not in
/// this list and not a server answer proves nothing, whatever it looks like:
/// a lookup that left the machine and got no answer in time also fails
/// locally.
pub const LOCAL_REFUSALS: [(u32, &str); 6] = [
    (5, "ERROR_ACCESS_DENIED"),
    (0x8007_0005, "E_ACCESSDENIED"),
    (1722, "RPC_S_SERVER_UNAVAILABLE"),
    (1753, "EPT_S_NOT_REGISTERED"),
    (9852, "DNS_ERROR_NO_DNS_SERVERS"),
    (10013, "WSAEACCES"),
];

/// The codes that say the lookup ran out of time. One of these can mean that
/// the query left the machine and the answer did not come back soon enough,
/// so it is never the confined side. The probe uses `-QuickTimeout`, which
/// makes this likely, not rare.
pub const TIMEOUTS: [(u32, &str); 2] = [(1460, "ERROR_TIMEOUT"), (10060, "WSAETIMEDOUT")];

fn named(table: &[(u32, &'static str)], code: u32) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

/// What the probe's own output proves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeClass {
    /// A DNS server answered: the lookup left the machine.
    Server,
    /// A named local failure: the lookup reached no server. The name of the
    /// status, for the report.
    LocalRefusal(&'static str),
    /// Neither. Nothing is proved, and the reason names the status.
    Unclear(String),
}

/// Which of the three [`ProbeClass`] cases `p` is.
///
/// There is no "anything that is not a server answer is a local failure"
/// case: a status that is in neither table is [`ProbeClass::Unclear`], which
/// never passes.
pub fn classify_probe(p: &ProbeResult) -> ProbeClass {
    let Some(code) = p.code else {
        // No status code. The error identifier can still prove a leak; it
        // never proves confinement, because a refusal and a server's name
        // error can look alike to a program.
        let id = p.error.split([',', ' ']).next().unwrap_or_default();
        if id.starts_with("DNS_ERROR_RCODE_") || id == "DNS_INFO_NO_RECORDS" {
            return ProbeClass::Server;
        }
        return ProbeClass::Unclear(format!(
            "the lookup in the sandbox printed no status, only the error {:?}",
            p.error
        ));
    };
    if is_server_answer(code) {
        return ProbeClass::Server;
    }
    if let Some(name) = named(&LOCAL_REFUSALS, code) {
        return ProbeClass::LocalRefusal(name);
    }
    if let Some(name) = named(&TIMEOUTS, code) {
        return ProbeClass::Unclear(format!(
            "the lookup in the sandbox ran out of time (status {code}, {name}), so it is not \
             known whether the query left the machine"
        ));
    }
    ProbeClass::Unclear(format!(
        "the lookup in the sandbox failed with status {code}, which is neither a DNS server's \
         answer nor a local refusal this check knows, so it proves nothing"
    ))
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
            // Also the truncated case: output that carried no end marker.
            return DnsVerdict::NotProved(format!(
                "the lookup in the sandbox left no usable evidence: {output}"
            ));
        }
    };
    let class = classify_probe(probe);
    if class == ProbeClass::Server {
        return DnsVerdict::Leaked(format!(
            "the sandbox user's lookup got a DNS server's answer ({})",
            describe(probe)
        ));
    }
    // The cache is checked before an unclear status is reported, so a leak
    // the cache saw is still called a leak.
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
    // Last: the probe itself must have failed for a named local reason. An
    // unclear status passes nothing, however clean the cache looks.
    match class {
        ProbeClass::LocalRefusal(_) => DnsVerdict::Confined,
        ProbeClass::Unclear(why) => DnsVerdict::NotProved(why),
        // Returned above.
        ProbeClass::Server => DnsVerdict::Leaked(describe(probe)),
    }
}

/// How long the probe may take before the check gives up on it.
///
/// Its longest legitimate holder is Windows PowerShell's own start-up: 5.1
/// loads the .NET runtime and builds the profile-less session, which on a
/// cold machine with a slow disk runs to tens of seconds. The lookup itself
/// is bounded by `-QuickTimeout`. 120 seconds is well past both, and far
/// under the two-hour bound a sandboxed command gets when it names none of
/// its own, which a hung probe would hold `kgraf sandbox selfcheck` for. A
/// probe that reaches the bound is `dns: not proved`, never an error that
/// throws away the rest of the check.
pub const PROBE_BOUND: std::time::Duration = std::time::Duration::from_secs(120);

/// How long the check waits before it reads the DNS Client cache a second
/// time.
///
/// The first reading is taken as soon as the probe's process has exited. A
/// query that left the machine can still be in flight then, so the cache
/// would not yet hold the name and the first reading would look confined.
/// Five seconds is longer than a DNS answer takes on a working network, and
/// short enough to add little to the check; the probe's own `-QuickTimeout`
/// gives up well inside it.
pub const CACHE_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// The verdict from a first reading of the evidence and, where that reading
/// passed, a second one taken after `settle`.
///
/// Only a pass is read twice: a leak and a `not proved` are already decided,
/// and a second reading cannot turn either into a pass. `wait` and `read` are
/// given by the caller so that the rule is testable without a clock.
pub fn judge_settled(
    settle: std::time::Duration,
    mut read: impl FnMut() -> DnsEvidence,
    mut wait: impl FnMut(std::time::Duration),
) -> DnsVerdict {
    let first = judge(&read());
    if !first.passed() {
        return first;
    }
    wait(settle);
    judge(&read())
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
            // A named local refusal: the only probe result that passes.
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
            "noise\r\n{CODE_MARK}9003\r\n{ERROR_MARK}DNS_ERROR_RCODE_NAME_ERROR,Microsoft.DnsClient.Commands.ResolveDnsName DNS name does not exist\r\n{END_MARK}\r\n"
        );
        let p = parse_probe(&out).unwrap();
        assert_eq!(p.code, Some(NAME_ERROR));
        assert_eq!(classify_probe(&p), ProbeClass::Server);
        let p = parse_probe(&format!(
            "{CODE_MARK}?\n{ERROR_MARK}DNS_ERROR_RCODE_NAME_ERROR,x y\n{END_MARK}"
        ))
        .unwrap();
        assert_eq!(p.code, None);
        assert_eq!(
            classify_probe(&p),
            ProbeClass::Server,
            "judged by the identifier"
        );
        let p = parse_probe(&format!("{CODE_MARK}-2147024891\n{END_MARK}")).unwrap();
        assert_eq!(p.code, Some(0x8007_0005));
        assert_eq!(
            classify_probe(&p),
            ProbeClass::LocalRefusal("E_ACCESSDENIED")
        );
        assert_eq!(parse_probe("powershell: not recognized"), None);
        assert_eq!(
            parse_probe(&format!("{CODE_MARK}0\n{END_MARK}"))
                .unwrap()
                .code,
            Some(0)
        );
    }

    /// A probe whose output does not carry the end marker is not evidence,
    /// and neither is one that carries it with no status and no error: both
    /// are `None`, which the caller turns into `not proved`.
    #[test]
    fn output_without_an_end_marker_or_a_witness_is_not_evidence() {
        // The exact shape of the defect: the status line cut off after the
        // marker. It used to parse as "a status was printed, code unknown",
        // which the old polarity read as a local failure.
        assert_eq!(parse_probe(CODE_MARK), None);
        assert_eq!(parse_probe(&format!("{CODE_MARK}\n{END_MARK}")), None);
        assert_eq!(parse_probe(&format!("{CODE_MARK}?\n{END_MARK}")), None);
        // A whole status line, but the output stops before the end marker.
        assert_eq!(parse_probe(&format!("{CODE_MARK}5")), None);
        assert_eq!(
            parse_probe(&format!("{CODE_MARK}5\n{ERROR_MARK}ERROR_ACCESS_DENIED,x")),
            None
        );
        assert_eq!(parse_probe(END_MARK), None, "an end and nothing else");
        // Whole output: a status, and the end marker last.
        let p = parse_probe(&format!("{CODE_MARK}5\n{END_MARK}")).unwrap();
        assert_eq!(p.code, Some(5));
        // The script prints the end marker on both paths.
        let name = probe_name().unwrap();
        let script = probe_script(&name).unwrap();
        assert!(script.contains(END_MARK));
        assert!(
            script.trim_end().ends_with(&format!("'{END_MARK}'")),
            "{script}"
        );
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

    /// Each status is sorted one of three ways, and only a named local
    /// refusal is the confined side.
    #[test]
    fn a_status_is_a_leak_a_named_refusal_or_nothing_proved() {
        let of = |code: u32| {
            classify_probe(&ProbeResult {
                code: Some(code),
                error: String::new(),
            })
        };
        for c in [0, 9001, NAME_ERROR, 9018, NO_RECORDS] {
            assert_eq!(of(c), ProbeClass::Server, "{c}");
        }
        for (code, name) in LOCAL_REFUSALS {
            assert_eq!(of(code), ProbeClass::LocalRefusal(name), "{code}");
        }
        // A timeout, and a status in neither table: named, and not a pass.
        for (code, name) in TIMEOUTS {
            let ProbeClass::Unclear(why) = of(code) else {
                panic!("{code} is not proof either way");
            };
            assert!(
                why.contains(&code.to_string()) && why.contains(name),
                "{why}"
            );
        }
        for code in [11001u32, 1234, 0x8007_0057] {
            let ProbeClass::Unclear(why) = of(code) else {
                panic!("{code} proves nothing");
            };
            assert!(why.contains(&code.to_string()), "{why}");
        }
        // No status and no named server error: nothing is proved, and the
        // reason carries what the probe did print.
        let p = ProbeResult {
            code: None,
            error: "SomethingElse,x y".into(),
        };
        let ProbeClass::Unclear(why) = classify_probe(&p) else {
            panic!("an unnamed error proves nothing");
        };
        assert!(why.contains("SomethingElse"), "{why}");
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
        assert!(
            v.to_string().contains("Resolve-DnsName"),
            "the verdict carries what the probe printed: {v}"
        );
        assert!(!v.passed());
        assert!(v.to_string().starts_with("dns: not proved"));
    }

    /// A cache read taken as soon as the probe's process exits can be too
    /// early: a query that left the machine may still be in flight, so the
    /// first reading looks confined. The verdict comes from a second reading
    /// taken after the settle wait.
    #[test]
    fn a_pass_is_read_a_second_time_after_the_settle_wait() {
        use std::cell::Cell;
        use std::time::Duration;

        let reads = Cell::new(0usize);
        let waited: Cell<Option<Duration>> = Cell::new(None);
        let read = || {
            reads.set(reads.get() + 1);
            let mut e = confined();
            if reads.get() > 1 {
                // The answer has arrived, and the service cached the name.
                e.cache_probe = e.cache_known;
            }
            e
        };
        let v = judge_settled(CACHE_SETTLE, read, |d| waited.set(Some(d)));
        assert!(
            matches!(v, DnsVerdict::Leaked(_)),
            "the second reading decides: {v:?}"
        );
        assert_eq!(reads.get(), 2);
        assert_eq!(waited.get(), Some(CACHE_SETTLE));
        // A verdict that is not a pass is decided at once: no wait, no
        // second reading.
        let reads = Cell::new(0usize);
        let waited = Cell::new(false);
        let read = || {
            reads.set(reads.get() + 1);
            let mut e = confined();
            e.control = NAME_ERROR;
            e.cache_probe = e.cache_known;
            e
        };
        let v = judge_settled(CACHE_SETTLE, read, |_| waited.set(true));
        assert!(matches!(v, DnsVerdict::Leaked(_)), "{v:?}");
        assert_eq!(reads.get(), 1);
        assert!(!waited.get());
        // The wait is long enough for a DNS answer on a working network.
        assert!(CACHE_SETTLE >= Duration::from_secs(2));
    }

    /// The probe's bound is its own, not the two-hour bound a sandboxed
    /// command gets when it names none.
    #[test]
    fn the_probe_bound_is_minutes_not_hours() {
        use std::time::Duration;
        assert!(PROBE_BOUND >= Duration::from_secs(60), "{PROBE_BOUND:?}");
        assert!(PROBE_BOUND <= Duration::from_secs(120), "{PROBE_BOUND:?}");
        // The bound a sandboxed command gets with no bound of its own.
        assert!(PROBE_BOUND < Duration::from_secs(2 * 60 * 60) / 10);
        // Long enough that the settle wait fits inside a check that keeps
        // its own patience.
        assert!(PROBE_BOUND > CACHE_SETTLE);
    }

    /// A clean cache and a probe status that proves no local failure is
    /// `not proved`, never `confined`. This is the shape of the defect: a
    /// probe with no status, judged "not a server answer, so confined".
    #[test]
    fn a_probe_with_no_named_local_failure_never_passes() {
        for code in [None, Some(1460), Some(10060), Some(11001), Some(1234)] {
            let mut e = confined();
            e.probe = Ok(ProbeResult {
                code,
                error: String::new(),
            });
            let v = judge(&e);
            assert!(
                matches!(v, DnsVerdict::NotProved(_)),
                "{code:?} must not pass: {v:?}"
            );
            assert!(!v.passed(), "{code:?}");
        }
        // A leak the cache saw is still a leak, whatever the status was.
        let mut e = confined();
        e.probe = Ok(ProbeResult {
            code: Some(1460),
            error: String::new(),
        });
        e.cache_probe = e.cache_known;
        assert!(matches!(judge(&e), DnsVerdict::Leaked(_)));
    }
}
