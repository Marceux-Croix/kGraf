//! The bundle manifest, and the rules that decide what the bundle runs.
//!
//! `scripts/package-wintest.sh` cross-builds the workspace's test binaries
//! and calls `kgraf-wintest pack` (built for the build machine), which reads
//! cargo's JSON messages through [`plan_from_messages`] and writes the
//! manifest. The runner on Windows reads the same manifest. The rules live
//! here, in one table each, so a reviewer sees in one place what runs, in
//! which set, and what is left out and why.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const MANIFEST_NAME: &str = "bundle-manifest.json";

/// Which run a test binary belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Set {
    /// The Windows-specific tests: always run, first.
    Core,
    /// Every other test that runs without Rust on the machine.
    Extended,
}

/// A test that is not run, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skip {
    /// A libtest `--skip` pattern (a substring of the test name).
    pub pattern: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestRun {
    /// `package/kind/name`, plus `#core` or `#rest` when one binary is split
    /// between the sets.
    pub id: String,
    pub package: String,
    pub kind: String,
    pub name: String,
    /// The binary inside the bundle, relative to the bundle folder.
    pub exe: String,
    /// The package folder relative to the source root: the working folder
    /// and `CARGO_MANIFEST_DIR` of the run, as `cargo test` sets them.
    pub package_dir: String,
    pub set: Set,
    /// libtest name filters; empty runs every test.
    pub filters: Vec<String>,
    pub skips: Vec<Skip>,
    /// Seconds each test is expected to take, for the time estimate.
    pub secs_per_test: f64,
}

/// A program a test starts by its build path (`CARGO_BIN_EXE_<name>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltProgram {
    pub name: String,
    /// Inside the bundle.
    pub exe: String,
    /// The path it was built at, relative to the source root.
    pub build_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotShipped {
    pub what: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub kgraf_version: String,
    pub git_commit: String,
    pub built_utc: String,
    pub target: String,
    /// The build machine's source path, as the test binaries hold it.
    pub build_root: String,
    pub runs: Vec<TestRun>,
    pub programs: Vec<BuiltProgram>,
    pub not_shipped: Vec<NotShipped>,
    /// Every top-level entry of the bundle folder, so the runner can tell an
    /// unzipped bundle from a folder that holds other things.
    pub bundle_entries: Vec<String>,
}

/// Test binaries left out of the bundle, by `package/kind/name` prefix.
pub const NOT_SHIPPED: &[(&str, &str)] = &[
    (
        "kgraf-host/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-shell/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-runtime/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-fleet/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-provider-anthropic/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-provider-openai/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-orchestrator/test/live_",
        "needs a model provider and its API key",
    ),
    (
        "kgraf-cli/test/tbench_bridge_live",
        "needs Docker and the Terminal-Bench harness",
    ),
    (
        "kgraf-tbench-bridge/test/live_docker_bridge",
        "needs Docker",
    ),
    (
        "kgraf-core/test/workspace_text",
        "checks the repository's own text; needs the full git checkout, not a Windows behavior",
    ),
    (
        "kgraf-provider/test/crosslimit_processes",
        "builds its helper program with cargo at run time; needs Rust",
    ),
    // trybuild compiles each fixture against the crate's source with cargo and
    // compares the compiler's words with a recorded file: it needs Rust and the
    // source tree, and a rustc release that rewords an error fails it. It is a
    // check of the code, not of Windows behavior.
    (
        "kgraf-interpret/test/compile_fail",
        "trybuild compiles fixtures from the source tree with cargo; needs Rust and the source",
    ),
    (
        "kgraf-provider/test/compile_fail",
        "trybuild compiles fixtures from the source tree with cargo; needs Rust and the source",
    ),
    (
        "kgraf-session/test/compile_fail",
        "trybuild compiles fixtures from the source tree with cargo; needs Rust and the source",
    ),
    (
        "kgraf-tools/test/compile_fail",
        "trybuild compiles fixtures from the source tree with cargo; needs Rust and the source",
    ),
    (
        "kgraf-mcp/test/stdio_live",
        "builds its MCP test server with cargo at run time; needs Rust",
    ),
    (
        "kgraf-mcp/test/http_live",
        "builds its MCP test server with cargo at run time; needs Rust",
    ),
];

/// Single tests skipped inside a shipped binary: `(package/kind/name, pattern, reason)`.
pub const SKIPPED_TESTS: &[(&str, &str, &str)] = &[
    (
        "kgraf-host/lib/kgraf_host",
        "each_attempt_gets_a_shell_bound_to_its_own_worktree",
        "builds the MCP test server with cargo at run time; needs Rust",
    ),
    (
        "kgraf-host/lib/kgraf_host",
        "an_mcp_tool_allowed_with_kgraf_allow_runs_in_an_unattended_delegate",
        "builds the MCP test server with cargo at run time; needs Rust",
    ),
    (
        "kgraf-diag/test/no_network_deps",
        "the_normal_dependency_tree_has_no_network_crate",
        "runs `cargo tree`; needs Rust",
    ),
    (
        "kgraf-orchestrator/lib/kgraf_orchestrator",
        "this_repository_with_ordinary_plans_is_not_refused",
        "reads this repository's git history; needs the full git checkout",
    ),
    (
        "kgraf-secrets/lib/kgraf_secrets",
        "persist_path_false_positive_regression_list_stays_unchanged",
        "lists the repository's tracked files with git; needs the full git checkout",
    ),
];

/// The Windows-specific tests. A binary named here with filters is split:
/// the filtered tests run in the core set and the rest in the extended set.
/// A binary named with no filters runs whole in the core set.
pub const CORE: &[(&str, &[&str], f64)] = &[
    // The Git Bash search, the argument transport, the network decision,
    // the dependency junctions, the restricted token's pure parts.
    ("kgraf-sandbox/lib/kgraf_sandbox", &[], 0.05),
    // sh and bash through Git Bash inside the sandbox.
    ("kgraf-sandbox/test/shell_windows", &[], 2.0),
    // Programs that load user32.dll, cmd, sh and bash start in the sandbox;
    // a cleared environment passes only what a program needs.
    ("kgraf-sandbox/test/startup_windows", &[], 1.0),
    // Writes outside, the profile, the registry, junctions, other sandboxes.
    ("kgraf-sandbox/test/escape_windows", &[], 2.0),
    // HTTPS inside the sandbox: the curl of Git for Windows (OpenSSL) works,
    // and System32 curl.exe (Schannel) is asserted to fail as documented.
    ("kgraf-sandbox/test/tls_windows", &[], 2.0),
    // Name lookups by the offline sandbox user never reach the DNS Client
    // service; the online user and the owner still resolve. Returns at once
    // without the sandbox user set up.
    ("kgraf-sandbox/test/dns_windows", &[], 5.0),
    // The runner's own process, threads and token refuse the command; the
    // command runs on a window station and desktop the owner made. Returns at
    // once without the sandbox user set up.
    ("kgraf-sandbox/test/runner_windows", &[], 2.0),
    // A command's pipe ends do not leak into concurrent spawns, and its
    // standard input is never the owner's console.
    ("kgraf-sandbox/test/stdio_windows", &[], 2.0),
    // The sandbox user setup end to end; runs only elevated with
    // KGRAF_SANDBOX_SETUP_TEST=1 (a hosted runner), else it says why.
    ("kgraf-cli/test/sandbox_setup_windows", &[], 1.0),
    ("kgraf-sandbox/test/workspace", &[], 0.5),
    ("kgraf-sandbox/test/merge", &[], 0.3),
    ("kgraf-sandbox/test/reopen", &[], 0.5),
    ("kgraf-sandbox/test/host_git", &[], 0.5),
    ("kgraf-sandbox/test/cargo_config", &[], 0.2),
    // The worker shell on Windows, the cmd and PowerShell readers.
    ("kgraf-tools/lib/kgraf_tools", &["windows"], 1.0),
    // The run-start network notice, dependency links, check integrity.
    (
        "kgraf-host/lib/kgraf_host",
        &[
            "network_notice",
            "dependency_links",
            "check_inputs",
            "check_network",
        ],
        0.5,
    ),
    ("kgraf-orchestrator/test/check_network_notice", &[], 1.0),
];

/// Expected seconds per test in the extended set (most tests are fast; the
/// estimate corrects itself from the observed rate).
pub const EXTENDED_SECS_PER_TEST: f64 = 0.15;

/// One test binary or program from cargo's `--message-format=json` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub package: String,
    pub kind: String,
    pub name: String,
    pub executable: String,
    pub manifest_dir: String,
    pub is_test: bool,
}

/// Reads `compiler-artifact` messages that carry an executable.
pub fn artifacts_from_messages(jsonl: &str) -> Vec<Artifact> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(exe) = v["executable"].as_str() else {
            continue;
        };
        if !seen.insert(exe.to_owned()) {
            continue;
        }
        let manifest = v["manifest_path"].as_str().unwrap_or_default();
        let manifest_dir = manifest
            .strip_suffix("/Cargo.toml")
            .or_else(|| manifest.strip_suffix("\\Cargo.toml"))
            .unwrap_or(manifest)
            .to_owned();
        let package = manifest_dir
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned();
        out.push(Artifact {
            package,
            kind: v["target"]["kind"][0].as_str().unwrap_or("?").to_owned(),
            name: v["target"]["name"].as_str().unwrap_or("?").to_owned(),
            executable: exe.to_owned(),
            manifest_dir,
            is_test: v["profile"]["test"].as_bool().unwrap_or(false),
        });
    }
    out
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Cuts `root` off the front of `path`. Both are compared with one
/// separator and without case, because a native Windows build reports
/// backslashes, a drive letter whose case may differ from the root's, and
/// sometimes the `\\?\` prefix. Each normalizing step keeps byte offsets, so
/// the result is a slice of the original.
fn relative<'a>(path: &'a str, root: &str) -> &'a str {
    fn norm(s: &str) -> String {
        s.replace('\\', "/").to_ascii_lowercase()
    }
    let verbatim = path.strip_prefix(r"\\?\").unwrap_or(path);
    let root = root.strip_prefix(r"\\?\").unwrap_or(root);
    let (p, r) = (norm(verbatim), norm(root));
    let r = r.trim_end_matches('/');
    match p.strip_prefix(r) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            verbatim[r.len()..].trim_start_matches(['/', '\\'])
        }
        _ => path,
    }
}

/// The runs, programs and left-out binaries for the artifacts of one build.
pub fn plan_from_messages(
    artifacts: &[Artifact],
    build_root: &str,
) -> (Vec<TestRun>, Vec<BuiltProgram>, Vec<NotShipped>) {
    let mut runs = Vec::new();
    let mut programs = Vec::new();
    let mut not_shipped = Vec::new();
    for a in artifacts {
        let key = format!("{}/{}/{}", a.package, a.kind, a.name);
        if !a.is_test {
            if a.kind == "bin" {
                programs.push(BuiltProgram {
                    name: a.name.clone(),
                    exe: format!("programs/{}", file_name(&a.executable)),
                    build_path: relative(&a.executable, build_root).to_owned(),
                });
            }
            continue;
        }
        if let Some((_, reason)) = NOT_SHIPPED.iter().find(|(p, _)| key.starts_with(p)) {
            not_shipped.push(NotShipped {
                what: key,
                reason: (*reason).to_owned(),
            });
            continue;
        }
        let skips: Vec<Skip> = SKIPPED_TESTS
            .iter()
            .filter(|(k, _, _)| *k == key)
            .map(|(_, pattern, reason)| Skip {
                pattern: (*pattern).to_owned(),
                reason: (*reason).to_owned(),
            })
            .collect();
        let base = TestRun {
            id: key.clone(),
            package: a.package.clone(),
            kind: a.kind.clone(),
            name: a.name.clone(),
            exe: format!("tests/{}", file_name(&a.executable)),
            package_dir: relative(&a.manifest_dir, build_root).to_owned(),
            set: Set::Extended,
            filters: Vec::new(),
            skips,
            secs_per_test: EXTENDED_SECS_PER_TEST,
        };
        match CORE.iter().find(|(k, _, _)| *k == key) {
            None => runs.push(base),
            Some((_, [], secs)) => runs.push(TestRun {
                set: Set::Core,
                secs_per_test: *secs,
                ..base
            }),
            Some((_, filters, secs)) => {
                let filters: Vec<String> = filters.iter().map(|f| (*f).to_owned()).collect();
                let mut rest_skips = base.skips.clone();
                rest_skips.extend(filters.iter().map(|f| Skip {
                    pattern: f.clone(),
                    reason: "runs in the core set".to_owned(),
                }));
                runs.push(TestRun {
                    id: format!("{key}#core"),
                    set: Set::Core,
                    filters,
                    secs_per_test: *secs,
                    ..base.clone()
                });
                runs.push(TestRun {
                    id: format!("{key}#rest"),
                    skips: rest_skips,
                    ..base
                });
            }
        }
    }
    // Core first, in the order of the CORE table; then the rest by id.
    let order = |r: &TestRun| {
        let core_pos = CORE
            .iter()
            .position(|(k, _, _)| r.set == Set::Core && r.id.split('#').next() == Some(*k))
            .unwrap_or(usize::MAX);
        (r.set, core_pos, r.id.clone())
    };
    runs.sort_by_key(order);
    (runs, programs, not_shipped)
}

/// Core binaries the CORE table names that the build did not produce: a
/// renamed test file must not silently drop out of the Windows run.
pub fn missing_core(runs: &[TestRun]) -> Vec<String> {
    CORE.iter()
        .map(|(k, _, _)| *k)
        .filter(|k| !runs.iter().any(|r| r.id.split('#').next() == Some(*k)))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(pkg: &str, kind: &str, name: &str, test: bool, exe: &str) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "manifest_path": format!("/src/k/crates/{pkg}/Cargo.toml"),
            "target": {"kind": [kind], "name": name},
            "profile": {"test": test},
            "executable": format!("/src/k/target/x86_64-pc-windows-msvc/debug/{exe}"),
        })
        .to_string()
    }

    fn fixture() -> String {
        [
            msg(
                "kgraf-sandbox",
                "test",
                "escape_windows",
                true,
                "deps/escape_windows-1.exe",
            ),
            msg(
                "kgraf-host",
                "lib",
                "kgraf_host",
                true,
                "deps/kgraf_host-2.exe",
            ),
            msg(
                "kgraf-host",
                "test",
                "live_sessions",
                true,
                "deps/live_sessions-3.exe",
            ),
            msg(
                "kgraf-graph",
                "test",
                "walk_rules",
                true,
                "deps/walk_rules-4.exe",
            ),
            msg(
                "kgraf-tools",
                "test",
                "compile_fail",
                true,
                "deps/compile_fail-5.exe",
            ),
            msg("kgraf-cli", "bin", "kgraf", false, "kgraf.exe"),
            msg("kgraf-cli", "lib", "kgraf_cli", false, "ignored.exe"),
            "{\"reason\":\"build-finished\",\"success\":true}".to_owned(),
            "not json".to_owned(),
        ]
        .join("\n")
    }

    #[test]
    fn binaries_are_sorted_into_sets_split_and_left_out_by_the_tables() {
        let arts = artifacts_from_messages(&fixture());
        let (runs, programs, not_shipped) = plan_from_messages(&arts, "/src/k");
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "kgraf-sandbox/test/escape_windows",
                "kgraf-host/lib/kgraf_host#core",
                "kgraf-graph/test/walk_rules",
                "kgraf-host/lib/kgraf_host#rest",
            ]
        );
        assert_eq!(runs[0].set, Set::Core);
        assert_eq!(runs[0].exe, "tests/escape_windows-1.exe");
        assert_eq!(runs[0].package_dir, "crates/kgraf-sandbox");
        let host_core = &runs[1];
        assert!(host_core.filters.contains(&"network_notice".to_owned()));
        let host_rest = &runs[3];
        assert_eq!(host_rest.set, Set::Extended);
        assert!(host_rest.filters.is_empty());
        // The rest skips what the core set runs, and the cargo-built tests.
        assert!(
            host_rest
                .skips
                .iter()
                .any(|s| s.pattern == "network_notice")
        );
        assert!(
            host_rest
                .skips
                .iter()
                .any(|s| s.reason.contains("needs Rust"))
        );
        assert_eq!(not_shipped.len(), 2);
        assert!(not_shipped[0].what.ends_with("live_sessions"));
        // A trybuild binary needs Rust and the source tree: left out, not run.
        assert!(not_shipped[1].what.ends_with("compile_fail"));
        assert!(not_shipped[1].reason.contains("needs Rust"));
        assert_eq!(
            programs,
            vec![BuiltProgram {
                name: "kgraf".into(),
                exe: "programs/kgraf.exe".into(),
                build_path: "target/x86_64-pc-windows-msvc/debug/kgraf.exe".into(),
            }]
        );
    }

    #[test]
    fn native_windows_paths_are_made_relative() {
        let root = r"D:\a\k\k";
        let exe = r"d:\a\k\k\target\wintest\debug\kgraf.exe";
        let want = r"target\wintest\debug\kgraf.exe";
        assert_eq!(relative(exe, root), want);
        // The root in forward slashes, the path in backslashes.
        assert_eq!(relative(exe, "D:/a/k/k"), want);
        // The verbatim prefix and a trailing separator.
        assert_eq!(
            relative(r"\\?\D:\a\k\k\crates\x", r"D:\a\k\k\"),
            r"crates\x"
        );
        // A sibling folder that shares the prefix is not inside the root.
        assert_eq!(relative(r"D:\a\k\k2\x", root), r"D:\a\k\k2\x");
        // The Unix spelling is unchanged.
        assert_eq!(relative("/src/k/crates/x", "/src/k"), "crates/x");
    }

    #[test]
    fn native_windows_messages_plan_relative_paths() {
        let line = serde_json::json!({
            "reason": "compiler-artifact",
            "manifest_path": r"D:\a\k\k\crates\kgraf-cli\Cargo.toml",
            "target": {"kind": ["bin"], "name": "kgraf"},
            "profile": {"test": false},
            "executable": r"D:\a\k\k\target\wintest\debug\kgraf.exe",
        })
        .to_string();
        let artifacts = artifacts_from_messages(&line);
        assert_eq!(artifacts[0].package, "kgraf-cli");
        let (_, programs, _) = plan_from_messages(&artifacts, r"D:\a\k\k");
        assert_eq!(programs[0].exe, "programs/kgraf.exe");
        assert_eq!(programs[0].build_path, r"target\wintest\debug\kgraf.exe");
    }

    #[test]
    fn a_core_binary_missing_from_the_build_is_reported() {
        let arts = artifacts_from_messages(&fixture());
        let (runs, _, _) = plan_from_messages(&arts, "/src/k");
        let missing = missing_core(&runs);
        assert!(missing.contains(&"kgraf-sandbox/test/shell_windows".to_owned()));
        assert!(!missing.contains(&"kgraf-sandbox/test/escape_windows".to_owned()));
    }
}
