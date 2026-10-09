//! DNS confinement of the Windows sandbox user (decision 0097): a name lookup
//! by an offline sandbox user must not reach the DNS Client service, and the
//! online user and the owner must still resolve names.
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for x86_64-pc-windows-msvc only. It
//! needs `kgraf sandbox setup` on the machine (set `KGRAF_SANDBOX_TAG` for a
//! tagged test setup), a working network, and administrator rights for
//! `Clear-DnsClientCache` and for reading WFP filters (a hosted Windows
//! runner has them). Without the sandbox user setup every test returns at
//! once and says so. Run:
//! `cargo test -p kgraf-sandbox --test dns_windows -- --test-threads=1 --nocapture`.
//!
//! The witness is the DNS Client service's cache (`Get-DnsClientCache`): the
//! service keeps a name error for a name it looked up, so a name it never
//! looked up is absent. Each test first proves that the cache lists a name
//! the owner looked up; where it does not, the test fails, because the
//! absence of the probe's name would prove nothing.
//!
//! The probe is this test binary itself: [`probe_entry`] does nothing in a
//! normal run, and when `KGRAF_DNS_PROBE` is set it does one lookup, prints
//! `PROBE=<result>` and exits 0.

#![cfg(windows)]

use std::net::ToSocketAddrs;

use kgraf_sandbox::sandbox_user::dns_check::{is_probe_name, is_server_answer, probe_name};
use kgraf_sandbox::sandbox_user::windows::{accounts, dns, wfp};
use kgraf_sandbox::sandbox_user::{OwnerNames, WfpKeys};
use kgraf_sandbox::{Backend, Network, SandboxPolicy, platform_isolation, sandboxed_command};

/// A name with real addresses, for the lookups that must still work.
const REAL_NAME: &str = "github.com";

// ---- probe side -------------------------------------------------------------

/// The in-sandbox half. A no-op unless `KGRAF_DNS_PROBE` is set.
#[test]
fn probe_entry() {
    let Ok(action) = std::env::var("KGRAF_DNS_PROBE") else {
        return;
    };
    let name = std::env::var("KGRAF_DNS_NAME").unwrap_or_default();
    let result = match action.as_str() {
        // The DNS Client API: the status code, 0 for an answer.
        "query" => dns::query(&name, false).to_string(),
        // What most programs call (getaddrinfo).
        "getaddrinfo" => match (name.as_str(), 443).to_socket_addrs() {
            Ok(addrs) if !addrs.as_slice().is_empty() => "ok".to_owned(),
            Ok(_) => "error no addresses".to_owned(),
            Err(e) => format!("error {e}"),
        },
        other => format!("error unknown probe {other}"),
    };
    println!("PROBE={result}");
    std::process::exit(0);
}

// ---- host side --------------------------------------------------------------

fn sandbox_user_mode() -> bool {
    platform_isolation().expect("a backend").backend == Backend::WindowsSandboxUser
}

fn owner_names() -> OwnerNames {
    let tag = std::env::var("KGRAF_SANDBOX_TAG")
        .ok()
        .filter(|t| !t.is_empty());
    let sid = accounts::current_user_sid().expect("this user's SID");
    OwnerNames::derive(&sid, tag.as_deref()).expect("the owner's names")
}

/// Run the probe `action` on `name` under `network`, as the sandbox user
/// that network selects, and return what it printed after `PROBE=`.
fn probe(network: Network, action: &str, name: &str) -> String {
    let tree = tempfile::tempdir().expect("a temporary tree");
    let root = kgraf_core::plain_path::canonicalize(tree.path()).expect("canonical tree");
    let policy = SandboxPolicy::new(vec![root.clone()], network).accept_partial_isolation(true);
    let out = sandboxed_command(&policy, std::env::current_exe().unwrap(), &root)
        .expect("the sandbox accepts the probe")
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_DNS_PROBE", action)
        .env("KGRAF_DNS_NAME", name)
        .output()
        .expect("run the probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("PROBE=").map(str::to_owned))
        .unwrap_or_else(|| {
            panic!(
                "the probe printed no result: status {:?}, stdout {stdout}, stderr {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            )
        })
}

/// Run a PowerShell command as the owner.
fn powershell(command: &str) -> std::process::Output {
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .output()
        .expect("run Windows PowerShell")
}

/// Whether the DNS Client service's cache lists `name`, as
/// `Get-DnsClientCache` shows it.
fn cache_lists(name: &str) -> bool {
    assert!(is_probe_name(name), "only probe names go into the script");
    let out = powershell(&format!(
        "@(Get-DnsClientCache | Where-Object {{ $_.Entry -like '*{name}*' }}).Count"
    ));
    let text = String::from_utf8_lossy(&out.stdout);
    let count: u32 = text.trim().parse().unwrap_or_else(|_| {
        panic!(
            "Get-DnsClientCache did not run: {text} {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    count > 0
}

/// Empty the DNS Client cache. Needs administrator rights; `false` when it
/// failed.
fn clear_cache() -> bool {
    powershell("Clear-DnsClientCache -ErrorAction Stop")
        .status
        .success()
}

/// The owner reaches a DNS server, and the cache lists a name the owner
/// looked up. Without both, the checks below prove nothing.
fn baseline() {
    let control = probe_name().unwrap();
    let code = dns::query(&control, false);
    assert!(
        is_server_answer(code),
        "no DNS server answered this machine's own lookup (status {code}); without a \
         network this test proves nothing"
    );
    assert!(
        cache_lists(&control),
        "the DNS Client cache does not list {control}, which this machine looked up; here the \
         cache cannot show whether the service looked a name up, so the test proves nothing \
         (use the DNS Client event log, Microsoft-Windows-DNS-Client/Operational, instead)"
    );
}

/// The DNS filter exists and a lookup by the offline (or slot) user never
/// reaches the DNS Client service: the lookup fails, and after
/// `Clear-DnsClientCache` the cache does not list the name.
#[test]
fn an_offline_lookup_does_not_reach_the_dns_client_service() {
    if !sandbox_user_mode() {
        eprintln!("skipped: the sandbox user is not set up on this machine");
        return;
    }
    let keys = WfpKeys::derive(&owner_names());
    match wfp::existing(&keys) {
        Ok(state) => {
            for (object, key) in keys.filters() {
                assert!(
                    state.present.contains(&key),
                    "the DNS filter on {} is missing; run `kgraf sandbox setup` again",
                    object.layer_words()
                );
                // Present is not effective: a filter the Base Filtering
                // Engine has disabled is read like any other.
                assert!(
                    !state.disabled.contains(&key),
                    "the DNS filter on {} carries FWPM_FILTER_FLAG_DISABLED, so it does not \
                     run; this is the case the reboot step of the test plan looks for",
                    object.layer_words()
                );
            }
        }
        Err(why) => eprintln!("note: the WFP filters were not read: {why}"),
    }
    if !clear_cache() {
        eprintln!(
            "note: Clear-DnsClientCache failed (it needs administrator rights); the probe \
             names are new, so the check still holds"
        );
    }
    baseline();

    let queried = probe_name().unwrap();
    let code = probe(Network::Deny, "query", &queried);
    let status: u32 = code.parse().unwrap_or_else(|_| panic!("probe said {code}"));
    assert!(
        !is_server_answer(status),
        "the offline user's DnsQuery got a DNS server's answer (status {status}): lookups \
         leave the machine"
    );
    let resolved = probe_name().unwrap();
    let line = probe(Network::Deny, "getaddrinfo", &resolved);
    assert_ne!(line, "ok", "the offline user resolved {resolved}");
    // The status the offline user's lookups got is the evidence the run
    // must report: it says how the filter's refusal looks to a program.
    eprintln!("offline user: DnsQuery status {status}, getaddrinfo {line}");

    for name in [&queried, &resolved] {
        assert!(
            !cache_lists(name),
            "the DNS Client service looked {name} up for the offline user: lookups leave the \
             machine"
        );
    }
}

/// The WFP layer each filter must sit on, as `netsh wfp show filters` names
/// it.
fn netsh_layer_of(object: kgraf_sandbox::sandbox_user::WfpObject) -> &'static str {
    use kgraf_sandbox::sandbox_user::WfpObject;
    match object {
        WfpObject::RpcFilter => "FWPM_LAYER_RPC_UM",
        WfpObject::NameCacheFilterV4 => "FWPM_LAYER_NAME_RESOLUTION_CACHE_V4",
        WfpObject::NameCacheFilterV6 => "FWPM_LAYER_NAME_RESOLUTION_CACHE_V6",
        other => panic!("{other} sits on no layer"),
    }
}

/// `netsh wfp show filters` lists each of kGraf's DNS filters, by its name,
/// key and layer. This is the view an administrator has of the filters,
/// independent of kGraf's own reading of them. Needs administrator rights;
/// without them the test says so and returns.
#[test]
fn netsh_lists_every_dns_filter_on_its_layer() {
    use kgraf_sandbox::sandbox_user::wfp::guid_text;
    if !sandbox_user_mode() {
        eprintln!("skipped: the sandbox user is not set up on this machine");
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary folder");
    let file = dir.path().join("filters.xml");
    let out = std::process::Command::new("netsh")
        .args(["wfp", "show", "filters"])
        .arg(format!("file={}", file.display()))
        .output()
        .expect("run netsh");
    let Ok(xml) = std::fs::read_to_string(&file) else {
        eprintln!(
            "skipped: netsh wrote no filter list (it needs administrator rights): {}",
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    };
    let lower = xml.to_ascii_lowercase();
    for (object, raw) in WfpKeys::derive(&owner_names()).filters() {
        let key = guid_text(raw);
        let at = lower.find(&key).unwrap_or_else(|| {
            panic!(
                "netsh does not list the DNS filter {key} on {}",
                object.layer_words()
            )
        });
        // The filter's own entry: from its key to the next filter's key. Its
        // conditions are nested items, so the end of the first item is not
        // the end of the entry.
        let end = lower[at..]
            .find("<filterkey>")
            .map_or(xml.len(), |e| at + e);
        let entry = &xml[at..end];
        eprintln!(
            "netsh entry of the DNS filter on {}:\n{entry}",
            object.layer_words()
        );
        for want in ["kGraf sandbox", netsh_layer_of(object), "FWP_ACTION_BLOCK"] {
            assert!(
                entry.contains(want),
                "the DNS filter's netsh entry lacks {want}:\n{entry}"
            );
        }
    }
}

/// The filter matches only the blocked users: the owner and the online user
/// still resolve names, and the online user's lookup goes through the DNS
/// Client service as before.
#[test]
fn the_online_user_and_the_owner_still_resolve() {
    if !sandbox_user_mode() {
        eprintln!("skipped: the sandbox user is not set up on this machine");
        return;
    }
    let own = (REAL_NAME, 443)
        .to_socket_addrs()
        .map(|mut a| a.next().is_some());
    assert!(
        matches!(own, Ok(true)),
        "the owner could not resolve {REAL_NAME}: {own:?}"
    );
    baseline();
    assert_eq!(
        probe(Network::Allow, "getaddrinfo", REAL_NAME),
        "ok",
        "the online user could not resolve {REAL_NAME}"
    );
    let name = probe_name().unwrap();
    let code = probe(Network::Allow, "query", &name);
    let status: u32 = code.parse().unwrap_or_else(|_| panic!("probe said {code}"));
    assert!(
        is_server_answer(status),
        "the online user's lookup reached no DNS server (status {status}); the DNS filter \
         must not match the online user"
    );
    assert!(
        cache_lists(&name),
        "the DNS Client cache does not list the online user's name {name}"
    );
}
