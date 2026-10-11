//! `--only <filter>`: run the named tests and nothing else.
//!
//! The filter is a comma-separated list. Each item is a manifest run id
//! (`kgraf-sandbox/test/escape_windows`), which runs that binary whole, or a
//! run id and one test, `<run id>::<full test name>`, which runs that test
//! alone (libtest `--exact`). A run id without its `#core` or `#rest` part
//! names both parts. The end-to-end checks run only when an item names them:
//! `e2e` runs them all, `e2e::<check id>` (`e2e::shell.git_bash`) keeps the
//! named check. The filter applies to the baseline suite and to the sandbox
//! user re-run alike.
//!
//! An item that names nothing in the manifest is an error before anything
//! runs: a filter that quietly ran less than it named would read as a pass.

use crate::manifest::TestRun;

/// The word that names the end-to-end checks in a filter.
pub const E2E: &str = "e2e";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    binary: String,
    test: Option<String>,
}

/// A parsed `--only` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Only {
    /// The value as given, for the report.
    pub text: String,
    items: Vec<Item>,
}

/// Which end-to-end checks a filter asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum E2eChoice {
    /// The filter names no end-to-end check: none runs.
    None,
    /// `e2e`: every check.
    All,
    /// `e2e::<id>` items: these checks, by id.
    Named(Vec<String>),
}

impl Only {
    /// Parse a filter. Empty items, an empty binary or test name, and a
    /// value with no item are errors.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut items = Vec::new();
        for raw in text.split(',') {
            let item = raw.trim();
            if item.is_empty() {
                return Err(format!(
                    "--only {text:?}: an empty item; give comma-separated `binary-id` or \
                     `binary-id::test-name` items"
                ));
            }
            let (binary, test) = match item.split_once("::") {
                Some((b, t)) => (b.trim(), Some(t.trim())),
                None => (item, None),
            };
            if binary.is_empty() || test.is_some_and(str::is_empty) {
                return Err(format!(
                    "--only {text:?}: the item {item:?} names no binary or no test"
                ));
            }
            items.push(Item {
                binary: binary.to_owned(),
                test: test.map(str::to_owned),
            });
        }
        Ok(Self {
            text: text.trim().to_owned(),
            items,
        })
    }

    fn names_run(binary: &str, id: &str) -> bool {
        binary == id || id.split('#').next() == Some(binary)
    }

    /// The manifest runs this filter selects, in manifest order. A run named
    /// with tests runs those tests alone (`--exact`); a run named whole runs
    /// as the manifest says. An item that names no run is an error.
    pub fn apply(&self, runs: &[TestRun]) -> Result<Vec<TestRun>, String> {
        for item in self.items.iter().filter(|i| i.binary != E2E) {
            if !runs.iter().any(|r| Self::names_run(&item.binary, &r.id)) {
                return Err(format!(
                    "--only names {:?}, which is not a test binary of this bundle; the run \
                     ids are listed in the report's Test binaries table",
                    item.binary
                ));
            }
        }
        let mut out = Vec::new();
        for r in runs {
            let named: Vec<&Item> = self
                .items
                .iter()
                .filter(|i| Self::names_run(&i.binary, &r.id))
                .collect();
            if named.is_empty() {
                continue;
            }
            let mut run = r.clone();
            if !named.iter().any(|i| i.test.is_none()) {
                let mut tests: Vec<String> = named.iter().filter_map(|i| i.test.clone()).collect();
                tests.dedup();
                run.filters = tests;
                run.exact = true;
            }
            out.push(run);
        }
        Ok(out)
    }

    /// Which end-to-end checks the filter asks for.
    pub fn e2e(&self) -> E2eChoice {
        let named: Vec<&Item> = self.items.iter().filter(|i| i.binary == E2E).collect();
        if named.is_empty() {
            E2eChoice::None
        } else if named.iter().any(|i| i.test.is_none()) {
            E2eChoice::All
        } else {
            E2eChoice::Named(named.iter().filter_map(|i| i.test.clone()).collect())
        }
    }
}

/// The end-to-end checks a filter keeps of those that ran. The checks run in
/// groups that share one worker run, so a named check runs with its group and
/// only it is kept. A named check that did not run is a failed check of its
/// own: the filter asked for something this run cannot show.
pub fn keep_checks(checks: Vec<crate::e2e::Check>, choice: &E2eChoice) -> Vec<crate::e2e::Check> {
    match choice {
        E2eChoice::None => Vec::new(),
        E2eChoice::All => checks,
        E2eChoice::Named(ids) => {
            let mut kept: Vec<crate::e2e::Check> =
                checks.into_iter().filter(|c| ids.contains(&c.id)).collect();
            for id in ids {
                if !kept.iter().any(|c| c.id == *id) {
                    kept.push(crate::e2e::Check {
                        id: id.clone(),
                        title: "an end-to-end check named by --only".to_owned(),
                        simulated: false,
                        status: crate::e2e::Status::Fail,
                        detail: format!(
                            "--only names the end-to-end check {id}, and no check of that id \
                             ran on this machine"
                        ),
                        evidence: String::new(),
                    });
                }
            }
            kept
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Set;

    fn ran(id: &str) -> crate::e2e::Check {
        crate::e2e::Check {
            id: id.into(),
            title: "t".into(),
            simulated: false,
            status: crate::e2e::Status::Pass,
            detail: String::new(),
            evidence: String::new(),
        }
    }

    #[test]
    fn only_the_named_checks_are_kept_and_a_missing_one_fails() {
        let all = || {
            vec![
                ran("shell.worker"),
                ran("shell.git_bash"),
                ran("git.present"),
            ]
        };
        assert!(keep_checks(all(), &E2eChoice::None).is_empty());
        assert_eq!(keep_checks(all(), &E2eChoice::All).len(), 3);
        let kept = keep_checks(
            all(),
            &E2eChoice::Named(vec!["shell.git_bash".into(), "shell.nope".into()]),
        );
        let ids: Vec<&str> = kept.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["shell.git_bash", "shell.nope"]);
        assert_eq!(kept[0].status, crate::e2e::Status::Pass);
        assert_eq!(kept[1].status, crate::e2e::Status::Fail);
    }

    fn run(id: &str) -> TestRun {
        TestRun {
            id: id.into(),
            package: "p".into(),
            kind: "test".into(),
            name: "n".into(),
            exe: "tests/n.exe".into(),
            package_dir: "crates/p".into(),
            set: Set::Extended,
            filters: Vec::new(),
            exact: false,
            skips: Vec::new(),
            secs_per_test: 0.15,
        }
    }

    fn runs() -> Vec<TestRun> {
        vec![
            run("kgraf-sandbox/test/escape_windows"),
            run("kgraf-host/lib/kgraf_host#core"),
            run("kgraf-host/lib/kgraf_host#rest"),
            run("kgraf-orchestrator/test/contract_checks"),
        ]
    }

    #[test]
    fn a_binary_runs_whole_and_a_named_test_runs_alone_and_exactly() {
        let only = Only::parse(
            "kgraf-orchestrator/test/contract_checks, \
             kgraf-sandbox/test/escape_windows::send_input_reaches_nothing_of_the_owners,\
             kgraf-sandbox/test/escape_windows::the_command_runs_on_its_own_desktop_without_a_console",
        )
        .expect("parses");
        let got = only.apply(&runs()).expect("applies");
        let ids: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "kgraf-sandbox/test/escape_windows",
                "kgraf-orchestrator/test/contract_checks"
            ]
        );
        assert!(got[0].exact);
        assert_eq!(
            got[0].filters,
            vec![
                "send_input_reaches_nothing_of_the_owners",
                "the_command_runs_on_its_own_desktop_without_a_console"
            ]
        );
        assert!(!got[1].exact && got[1].filters.is_empty());
        assert_eq!(only.e2e(), E2eChoice::None);
    }

    #[test]
    fn a_run_id_without_its_part_names_both_parts() {
        let only = Only::parse("kgraf-host/lib/kgraf_host").expect("parses");
        let got = only.apply(&runs()).expect("applies");
        let ids: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "kgraf-host/lib/kgraf_host#core",
                "kgraf-host/lib/kgraf_host#rest"
            ]
        );
        let one = Only::parse("kgraf-host/lib/kgraf_host#rest::tests::x").expect("parses");
        let got = one.apply(&runs()).expect("applies");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "kgraf-host/lib/kgraf_host#rest");
        assert_eq!(got[0].filters, vec!["tests::x"]);
    }

    #[test]
    fn a_whole_binary_item_wins_over_a_test_item_of_the_same_binary() {
        let only = Only::parse(
            "kgraf-orchestrator/test/contract_checks::a,kgraf-orchestrator/test/contract_checks",
        )
        .expect("parses");
        let got = only.apply(&runs()).expect("applies");
        assert!(!got[0].exact && got[0].filters.is_empty());
    }

    #[test]
    fn the_end_to_end_checks_run_only_when_named() {
        assert_eq!(Only::parse("e2e").expect("p").e2e(), E2eChoice::All);
        assert_eq!(
            Only::parse("e2e::shell.git_bash,e2e::git.present")
                .expect("p")
                .e2e(),
            E2eChoice::Named(vec!["shell.git_bash".into(), "git.present".into()])
        );
        assert_eq!(
            Only::parse("e2e::shell.git_bash,e2e").expect("p").e2e(),
            E2eChoice::All
        );
        // `e2e` is not a binary: naming it alone selects no test binary.
        assert!(
            Only::parse("e2e")
                .expect("p")
                .apply(&runs())
                .expect("applies")
                .is_empty()
        );
    }

    #[test]
    fn a_filter_that_names_nothing_or_is_malformed_is_refused() {
        for bad in ["", " ", "a,,b", "::t", "a::", "a, "] {
            assert!(Only::parse(bad).is_err(), "{bad:?}");
        }
        let unknown = Only::parse("kgraf-sandbox/test/escape_window").expect("parses");
        let err = unknown.apply(&runs()).expect_err("unknown binary");
        assert!(err.contains("kgraf-sandbox/test/escape_window"), "{err}");
    }
}
