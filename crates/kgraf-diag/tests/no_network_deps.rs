//! "Nothing here is ever sent anywhere" (this crate's own
//! doc, decision 0069) is a load-bearing claim, not just a design
//! intention. This checks it mechanically: `kgraf-diag`'s own (non-dev)
//! dependency tree must carry no crate capable of a network call, so a future
//! dependency addition that would violate it fails the build instead of
//! silently landing.

use std::process::Command;

/// Crate names that mean "this can talk to the network", directly or as the
/// transport underneath something that can. Not exhaustive by design (this
/// is a tripwire, not a full supply-chain audit); it covers the common HTTP,
/// TLS and async-runtime building blocks a future PR would actually pull in
/// if diagnostics ever grew a network path by accident.
const BLOCKLIST: &[&str] = &[
    "reqwest",
    "hyper",
    "tokio",
    "h2",
    "h3",
    "rustls",
    "native-tls",
    "openssl",
    "openssl-sys",
    "curl",
    "curl-sys",
    "ureq",
    "tonic",
    "surf",
    "isahc",
    "async-std",
    "async-h1",
];

#[test]
fn the_normal_dependency_tree_has_no_network_crate() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--offline",
            "-p",
            "kgraf-diag",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree must run");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    let dep_names: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.trim().split(' ').next())
        .collect();
    for name in BLOCKLIST {
        assert!(
            !dep_names.contains(name),
            "kgraf-diag's dependency tree includes network-capable crate `{name}`; \
             this crate's whole reason to exist is that nothing in it can reach the \
             network (decision 0069):\n{tree}"
        );
    }
}
