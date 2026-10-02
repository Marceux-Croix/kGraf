//! Plan checks that need the repository as it is when the plan is made.
//!
//! [`crate::plan::validate_plan_with`] is pure: it sees the contracts and
//! nothing else. Two defects of a plan show only against the repository, and
//! each leaves a contract that cannot pass however the worker tries:
//!
//! - **An unwritten scaffold.** A file that holds placeholder bodies
//!   (`todo!()`, `unimplemented!()`, `raise NotImplementedError`) and that the
//!   instruction names, but that no contract writes. The work it stands for is
//!   then done by nobody, or a worker meets the file outside its write scope
//!   and the contract is held.
//! - **A workspace the acceptance builds whole before its members exist.** A
//!   contract's acceptance runs `cargo build|test|check|clippy --workspace`
//!   (or `--all`), but a member of the workspace has no manifest yet and is
//!   written by no contract, or only by a contract this one does not depend on.
//!   The build fails on the missing manifest, and the contract that would
//!   create it never runs first.
//!
//! Both are refused as [`PlanError::Invalid`], which the planner is asked
//! again with; the reason names the file and the fix.

use crate::plan::{ContractSpec, PlanError};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The most a file is read, in bytes, to look for placeholder bodies.
const MAX_READ_BYTES: u64 = 256 * 1024;

/// The repository files a check looks at.
pub(crate) trait RepoView {
    fn exists(&self, rel: &str) -> bool;
    /// The file's text, `None` when it is absent, unreadable or not UTF-8.
    fn read(&self, rel: &str) -> Option<String>;
}

/// The repository checkout on disk.
pub(crate) struct DiskRepo(pub PathBuf);

impl RepoView for DiskRepo {
    fn exists(&self, rel: &str) -> bool {
        self.0.join(rel).is_file()
    }

    fn read(&self, rel: &str) -> Option<String> {
        use std::io::Read;
        let file = std::fs::File::open(self.0.join(rel)).ok()?;
        let mut text = String::new();
        file.take(MAX_READ_BYTES).read_to_string(&mut text).ok()?;
        Some(text)
    }
}

/// The first refusal of `specs` against the repository, if any. `named` are
/// the repository files the instruction names (`planner_context`).
pub(crate) fn refusal(
    specs: &[ContractSpec],
    named: &[String],
    repo: &dyn RepoView,
) -> Option<PlanError> {
    scaffold_refusal(specs, named, repo)
        .or_else(|| workspace_refusal(specs, repo))
        .map(PlanError::Invalid)
}

fn norm(path: &str) -> &str {
    path.trim_start_matches("./")
}

fn writes_of(specs: &[ContractSpec]) -> BTreeMap<&str, Vec<&str>> {
    let mut by_path: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for c in specs {
        for w in &c.writes {
            by_path.entry(norm(&w.path)).or_default().push(&c.id);
        }
    }
    by_path
}

/// Whether `text` holds a placeholder body on a line that is code: a comment
/// line that mentions one (a doc line, a note) does not count.
pub(crate) fn has_placeholder_body(text: &str) -> bool {
    text.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('#') || t.starts_with("/*") || t.starts_with('*') {
            return false;
        }
        t.contains("todo!(")
            || t.contains("unimplemented!(")
            || t.contains("raise NotImplementedError")
    })
}

/// A named file that holds placeholder bodies and that no contract writes.
fn scaffold_refusal(
    specs: &[ContractSpec],
    named: &[String],
    repo: &dyn RepoView,
) -> Option<String> {
    let written = writes_of(specs);
    let mut unwritten: Vec<&str> = Vec::new();
    for file in named {
        let file = norm(file);
        if written.contains_key(file) || unwritten.contains(&file) {
            continue;
        }
        if repo.read(file).is_some_and(|t| has_placeholder_body(&t)) {
            unwritten.push(file);
        }
    }
    let first = *unwritten.first()?;
    let list = unwritten
        .iter()
        .map(|f| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{list} holds placeholder bodies (todo!(), unimplemented!() or NotImplementedError) \
         that the instruction names, but no contract writes {}. Its work would be done by \
         nobody, or a worker would meet the file outside its write scope. Add `{first}` to the \
         writes of the contract that implements it, or add a contract for it",
        if unwritten.len() == 1 { "it" } else { "them" }
    ))
}

/// The contracts `id` depends on, directly or through others, `id` excluded.
fn ancestors<'a>(specs: &'a [ContractSpec], id: &str) -> BTreeSet<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = vec![id];
    while let Some(next) = stack.pop() {
        let Some(c) = specs.iter().find(|c| c.id == next) else {
            continue;
        };
        for d in &c.depends_on {
            if let Some(dep) = specs.iter().find(|c| &c.id == d)
                && seen.insert(&dep.id)
            {
                stack.push(&dep.id);
            }
        }
    }
    seen
}

/// Whether `acceptance` builds a whole Cargo workspace.
fn builds_whole_workspace(acceptance: &str) -> Option<String> {
    let words: Vec<&str> = acceptance
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | ',' | ';' | '\'' | '"'))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, w) in words.iter().enumerate() {
        if *w != "cargo" {
            continue;
        }
        let Some(verb) = words.get(i + 1) else {
            continue;
        };
        if !matches!(*verb, "build" | "test" | "check" | "clippy") {
            continue;
        }
        // The flags up to the next command or the `--` that starts the
        // arguments of the program cargo runs.
        let flag = words[i + 2..]
            .iter()
            .take_while(|w| !matches!(**w, "cargo" | "&&" | "||" | "--"))
            .find(|w| **w == "--workspace" || **w == "--all");
        if let Some(flag) = flag {
            return Some(format!("cargo {verb} {flag}"));
        }
    }
    None
}

/// The member directories a workspace names in `text`: the quoted strings of
/// the list after `members`, as `members = ["a", "b"]` or `members: ["a"]`
/// reads. A glob member is not listed.
fn members_in(text: &str) -> Vec<String> {
    let Some(at) = text.find("members") else {
        return Vec::new();
    };
    let after = &text[at + "members".len()..];
    let Some(open) = after.find('[') else {
        return Vec::new();
    };
    // The list must follow `members` closely: `members = [` or `members: [`.
    let between =
        after[..open].trim_matches(|c: char| c.is_whitespace() || matches!(c, '=' | ':' | '`'));
    if !between.is_empty() {
        return Vec::new();
    }
    let Some(close) = after[open..].find(']') else {
        return Vec::new();
    };
    // Split on quotes: the odd pieces are the quoted strings.
    after[open + 1..open + close]
        .split(['"', '\''])
        .skip(1)
        .step_by(2)
        .map(|m| m.trim().trim_start_matches("./").trim_end_matches('/'))
        .filter(|m| !m.is_empty() && !m.contains('*'))
        .map(str::to_owned)
        .collect()
}

/// The members of the workspace a contract writes a manifest for, with the
/// manifest path of each: from the manifest already in the repository when it
/// has `[workspace]` members, else from the members the contract's
/// instruction lists.
fn workspace_members(
    spec: &ContractSpec,
    manifest: &str,
    repo: &dyn RepoView,
) -> Vec<(String, String)> {
    let base = manifest.strip_suffix("Cargo.toml").unwrap_or("");
    let listed: Vec<String> = match repo
        .read(manifest)
        .and_then(|t| t.parse::<toml::Table>().ok())
    {
        Some(table) => table
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array())
            .map(|m| {
                m.iter()
                    .filter_map(|x| x.as_str())
                    .map(|m| m.trim_start_matches("./").trim_end_matches('/').to_owned())
                    .filter(|m| !m.contains('*'))
                    .collect()
            })
            .unwrap_or_default(),
        // Only the root manifest: a nested one the plan writes is a member's,
        // and the instruction's member list is not about it.
        None if manifest == "Cargo.toml" => members_in(&spec.instruction),
        None => Vec::new(),
    };
    listed
        .into_iter()
        .map(|m| (m.clone(), format!("{base}{m}/Cargo.toml")))
        .collect()
}

/// A contract whose acceptance builds the whole workspace although a member's
/// manifest is neither in the repository nor written by that contract or one
/// it depends on, or although the workspace's own manifest is not.
fn workspace_refusal(specs: &[ContractSpec], repo: &dyn RepoView) -> Option<String> {
    let written = writes_of(specs);
    // Every workspace the plan writes the manifest of, with its members.
    let mut workspaces: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for c in specs {
        for w in &c.writes {
            let path = norm(&w.path);
            if path.rsplit('/').next() == Some("Cargo.toml") {
                let members = workspace_members(c, path, repo);
                if !members.is_empty() {
                    workspaces.push((path.to_owned(), members));
                }
            }
        }
    }
    if workspaces.is_empty() {
        return None;
    }
    for c in specs {
        let Some(command) = builds_whole_workspace(&c.acceptance) else {
            continue;
        };
        let mut supplied: BTreeSet<&str> = ancestors(specs, &c.id);
        supplied.insert(&c.id);
        // The paths this contract can count on: in the repository, or written
        // by it or by a contract it depends on.
        let reachable = |path: &str| {
            repo.exists(path)
                || written
                    .get(path)
                    .is_some_and(|ids| ids.iter().any(|id| supplied.contains(id)))
        };
        let writers = |path: &str| -> String {
            match written.get(path) {
                None => "by no contract".to_owned(),
                Some(ids) => format!(
                    "only by contract{} {}, which `{}` does not depend on",
                    if ids.len() == 1 { "" } else { "s" },
                    ids.iter()
                        .map(|i| format!("`{i}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    c.id
                ),
            }
        };
        for (manifest, members) in &workspaces {
            if !reachable(manifest) {
                return Some(format!(
                    "contract `{}` acceptance builds the whole workspace (`{command}`), but \
                     the workspace manifest `{manifest}` is not in the repository and is \
                     written {}, so that command fails when `{}` runs. Make `{}` write \
                     `{manifest}` or depend on the contract that does, or give it an \
                     acceptance that builds only its own package (`cargo test -p <package>`)",
                    c.id,
                    writers(manifest),
                    c.id,
                    c.id
                ));
            }
            for (member, member_manifest) in members {
                if reachable(member_manifest) {
                    continue;
                }
                return Some(format!(
                    "contract `{}` acceptance builds the whole workspace (`{command}`), but \
                     the workspace `{manifest}` lists member `{member}` and `{member_manifest}` \
                     does not exist and is written {}, so cargo fails on the missing manifest \
                     when `{}` runs. Make `{}` also write `{member_manifest}` (and the \
                     member's sources), make it depend on the contract that does, or give it \
                     an acceptance that builds only its own package (`cargo test -p \
                     <package>`)",
                    c.id,
                    writers(member_manifest),
                    c.id,
                    c.id
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_state::WriteTarget;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake(HashMap<String, String>);

    impl Fake {
        fn with(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
                    .collect(),
            )
        }
    }

    impl RepoView for Fake {
        fn exists(&self, rel: &str) -> bool {
            self.0.contains_key(rel)
        }
        fn read(&self, rel: &str) -> Option<String> {
            self.0.get(rel).cloned()
        }
    }

    fn spec(
        id: &str,
        instruction: &str,
        acceptance: &str,
        writes: &[&str],
        deps: &[&str],
    ) -> ContractSpec {
        ContractSpec {
            id: id.into(),
            instruction: instruction.into(),
            acceptance: acceptance.into(),
            writes: writes
                .iter()
                .map(|p| WriteTarget {
                    path: (*p).into(),
                    node: None,
                })
                .collect(),
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            slices: vec![],
            tools: vec![],
            route: None,
            attempts: 1,
            task_kind: None,
        }
    }

    fn reason(r: Option<PlanError>) -> String {
        match r.expect("refused") {
            PlanError::Invalid(reason) => reason,
            other => panic!("{other:?}"),
        }
    }

    const STUB: &str = "use catalog::flags_for;\n\npub struct DiscountClearance;\n\nimpl Discount for DiscountClearance {\n    fn discount_cents(&self) -> i64 {\n        todo!(\"discount_clearance contract: implement\")\n    }\n}\n";

    /// Round evidence (conflicts-tier2): the plan gave the clearance work to a
    /// contract that wrote only `catalog/src/lib.rs`; no contract wrote the
    /// stub `discount_clearance/src/lib.rs`, and the worker was held at it.
    #[test]
    fn a_named_stub_no_contract_writes_is_refused_with_the_file_and_the_fix() {
        let repo = Fake::with(&[("discount_clearance/src/lib.rs", STUB)]);
        let specs = [
            spec(
                "c4",
                "add CLR-1 to the clearance list",
                "cargo test -p catalog",
                &["catalog/src/lib.rs"],
                &[],
            ),
            spec(
                "c1",
                "tax",
                "cargo test -p tax_us2",
                &["tax_us2/src/lib.rs"],
                &[],
            ),
        ];
        let named = vec!["discount_clearance/src/lib.rs".to_owned()];
        let why = reason(refusal(&specs, &named, &repo));
        assert!(why.contains("`discount_clearance/src/lib.rs`"), "{why}");
        assert!(why.contains("no contract writes it"), "{why}");
        assert!(
            why.contains("Add `discount_clearance/src/lib.rs` to the writes"),
            "{why}"
        );
        // Written by a contract: accepted.
        let mut ok = specs.to_vec();
        ok[0].writes.push(WriteTarget {
            path: "./discount_clearance/src/lib.rs".into(),
            node: None,
        });
        assert!(refusal(&ok, &named, &repo).is_none());
    }

    #[test]
    fn only_a_named_file_with_a_placeholder_in_code_counts() {
        let repo = Fake::with(&[
            (
                "a/src/lib.rs",
                "// remember: no todo!() left\npub fn f() {}\n",
            ),
            ("b/src/lib.rs", "pub fn f() { todo!() }\n"),
            ("c/mod.py", "def f():\n    raise NotImplementedError\n"),
            ("d/src/lib.rs", "pub fn f() { unimplemented!() }\n"),
        ]);
        let specs = [spec("c1", "x", "cargo test -p z", &["z/src/lib.rs"], &[])];
        // A comment that names the macro is not a placeholder.
        assert!(refusal(&specs, &["a/src/lib.rs".into()], &repo).is_none());
        for stub in ["b/src/lib.rs", "c/mod.py", "d/src/lib.rs"] {
            let why = reason(refusal(&specs, &[stub.into()], &repo));
            assert!(why.contains(stub), "{why}");
        }
        // A file the instruction does not name is not looked at, and a named
        // file that is absent has nothing to refuse.
        assert!(refusal(&specs, &[], &repo).is_none());
        assert!(refusal(&specs, &["gone/src/lib.rs".into()], &repo).is_none());
    }

    /// Round evidence (greenfield): c1 writes `Cargo.toml` listing members
    /// `wcl-core` and `wcl` and accepts on `cargo test --workspace`; `wcl` is
    /// written only by c2, which c1 does not depend on. The attempt could not
    /// pass.
    fn greenfield(c1_deps: &[&str], c2_deps: &[&str]) -> [ContractSpec; 2] {
        [
            spec(
                "c1",
                "Create the Cargo workspace at the repository root with `Cargo.toml` specifying members `[\"wcl-core\", \"wcl\"]`. Create the `wcl-core` library crate.",
                "Run `cargo test --workspace` locally; it must pass instantly with zero external dependencies and no network access.",
                &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"],
                c1_deps,
            ),
            spec(
                "c2",
                "Create the `wcl` binary crate.",
                "Run `cargo build --workspace` and `cargo test --workspace`; both must succeed without errors.",
                &["wcl/Cargo.toml", "wcl/src/main.rs"],
                c2_deps,
            ),
        ]
    }

    #[test]
    fn a_workspace_acceptance_with_a_member_another_contract_writes_is_refused() {
        let repo = Fake::default();
        let why = reason(refusal(&greenfield(&[], &[]), &[], &repo));
        assert!(why.contains("`c1`"), "{why}");
        assert!(why.contains("cargo test --workspace"), "{why}");
        assert!(why.contains("`wcl/Cargo.toml`"), "{why}");
        assert!(
            why.contains("only by contract `c2`, which `c1` does not depend on"),
            "{why}"
        );
        assert!(why.contains("also write `wcl/Cargo.toml`"), "{why}");
    }

    #[test]
    fn a_member_written_by_nobody_is_named_as_such() {
        let repo = Fake::default();
        let mut specs = greenfield(&[], &[]);
        specs[1].writes.clear();
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("written by no contract"), "{why}");
        assert!(why.contains("`wcl/Cargo.toml`"), "{why}");
    }

    #[test]
    fn a_workspace_acceptance_is_accepted_when_the_members_come_first_or_exist() {
        // The member's contract is a dependency of the one that builds the
        // whole workspace; the member's own acceptance builds its package only.
        let mut specs = greenfield(&["c2"], &[]);
        specs[1].acceptance = "cargo test -p wcl".into();
        assert!(
            refusal(&specs, &[], &Fake::default()).is_none(),
            "{:?}",
            refusal(&specs, &[], &Fake::default())
        );
        // The member already exists in the repository.
        let repo = Fake::with(&[("wcl/Cargo.toml", "[package]\nname = \"wcl\"\n")]);
        let mut specs = greenfield(&[], &[]);
        specs[1].acceptance = "cargo test -p wcl".into();
        assert!(refusal(&specs, &[], &repo).is_none());
        // One contract writes the whole workspace.
        let one = [spec(
            "c1",
            "Create the workspace: `Cargo.toml` with members = [\"a\", \"b\"]",
            "cargo build --workspace",
            &["Cargo.toml", "a/Cargo.toml", "b/Cargo.toml"],
            &[],
        )];
        assert!(refusal(&one, &[], &Fake::default()).is_none());
    }

    #[test]
    fn a_workspace_manifest_only_a_later_contract_writes_is_refused() {
        // c2's acceptance builds the workspace whose root manifest only c1 writes.
        let repo = Fake::default();
        let mut specs = greenfield(&[], &[]);
        specs[0].acceptance = "cargo test -p wcl-core".into();
        specs[0].writes.push(WriteTarget {
            path: "wcl/Cargo.toml".into(),
            node: None,
        });
        specs[1].writes.retain(|w| w.path != "wcl/Cargo.toml");
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("`c2`"), "{why}");
        assert!(why.contains("workspace manifest `Cargo.toml`"), "{why}");
    }

    #[test]
    fn an_existing_workspace_lists_its_members_from_the_manifest() {
        let repo = Fake::with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"crates/*\", \"b\"]\n",
            ),
            ("a/Cargo.toml", "[package]\nname=\"a\"\n"),
        ]);
        let specs = [spec(
            "c1",
            "edit the workspace",
            "cargo test --workspace",
            &["Cargo.toml"],
            &[],
        )];
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("`b/Cargo.toml`"), "{why}");
        assert!(!why.contains("crates"), "{why}");
    }

    #[test]
    fn the_command_and_member_readers_are_exact() {
        assert_eq!(
            builds_whole_workspace("run `cargo test --workspace` locally").as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(
            builds_whole_workspace("cargo clippy --all -- -D warnings").as_deref(),
            Some("cargo clippy --all")
        );
        for no in [
            "cargo test -p wcl-core",
            "cargo build",
            "cargo fmt --all",
            "the workspace builds",
            "cargo test -p a && echo --workspace",
        ] {
            assert_eq!(builds_whole_workspace(no), None, "{no}");
        }
        assert_eq!(
            members_in("with members `[\"wcl-core\", \"wcl\"]` and more"),
            vec!["wcl-core".to_owned(), "wcl".to_owned()]
        );
        assert_eq!(members_in("members = ['a']"), vec!["a".to_owned()]);
        assert!(members_in("members of the team [\"x\"]").is_empty());
        assert!(members_in("members = [\"crates/*\"]").is_empty());
    }
}
