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
//! - **A cargo acceptance in a workspace whose members are not there yet.**
//!   Cargo loads the manifest of every workspace member before it runs any
//!   command, whatever its form: `--workspace`, `-p <package>`,
//!   `--manifest-path <member>/Cargo.toml`, or a bare command at the root or
//!   in a member's directory. A contract whose acceptance runs cargo in a
//!   workspace that lists a member whose manifest (or any Rust source) is
//!   neither in the repository nor written by that contract or one it depends
//!   on fails on the missing manifest, and the contract that would create it
//!   never runs first. So does a command at the workspace's root when the
//!   workspace manifest itself comes later.
//!
//! Both are refused as [`PlanError::Invalid`], which the planner is asked
//! again with; the reason names the file and a fix measured to build. A
//! worker that meets the second defect anyway and asks for the missing file
//! is held for a re-plan ([`member_hold`]).
//!
//! Go workspaces and Maven reactors load the same way: a `go` command loads
//! every module its `go.work` uses, and `mvn` every module its `pom.xml`
//! lists, so a member whose `go.mod` or `pom.xml` comes later is the same
//! defect ([`module_findings`]). Other workspace tools were measured for the
//! same load and do not have it: uv 0.11 skips a listed member with no
//! `pyproject.toml`, and npm 11 runs with a listed workspace directory
//! missing (only `--workspace=<dir>` of the missing one fails). Gradle and
//! pnpm are not checked: whether a listed project with no directory fails
//! depends on their version.

use crate::placeholder::has_placeholder_body;
use crate::plan::{ContractSpec, PlanError};
use crate::run_state::OpenContract;
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

fn writes_of<C: PlanContract>(specs: &[C]) -> BTreeMap<&str, Vec<&str>> {
    let mut by_path: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for c in specs {
        for w in c.write_paths() {
            by_path.entry(norm(w)).or_default().push(c.id());
        }
    }
    by_path
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
        // Only a placeholder in code counts: the words in a string literal or a
        // comment are text about one (`crate::placeholder`).
        if repo
            .read(file)
            .is_some_and(|t| has_placeholder_body(file, &t))
        {
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

/// A contract as the workspace check reads it: a plan's [`ContractSpec`] at
/// intake, or an [`OpenContract`] when a worker reports a shortfall.
pub(crate) trait PlanContract {
    fn id(&self) -> &str;
    fn instruction(&self) -> &str;
    fn acceptance(&self) -> &str;
    fn write_paths(&self) -> Vec<&str>;
    fn dependencies(&self) -> &[String];
}

impl PlanContract for ContractSpec {
    fn id(&self) -> &str {
        &self.id
    }
    fn instruction(&self) -> &str {
        &self.instruction
    }
    fn acceptance(&self) -> &str {
        &self.acceptance
    }
    fn write_paths(&self) -> Vec<&str> {
        self.writes.iter().map(|w| w.path.as_str()).collect()
    }
    fn dependencies(&self) -> &[String] {
        &self.depends_on
    }
}

impl PlanContract for OpenContract {
    fn id(&self) -> &str {
        &self.contract
    }
    fn instruction(&self) -> &str {
        &self.instruction
    }
    fn acceptance(&self) -> &str {
        &self.acceptance
    }
    fn write_paths(&self) -> Vec<&str> {
        self.writes.iter().map(|w| w.path.as_str()).collect()
    }
    fn dependencies(&self) -> &[String] {
        &self.depends_on
    }
}

/// The contracts `id` depends on, directly or through others, `id` excluded.
fn ancestors<'a, C: PlanContract>(specs: &'a [C], id: &str) -> BTreeSet<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = vec![id];
    while let Some(next) = stack.pop() {
        let Some(c) = specs.iter().find(|c| c.id() == next) else {
            continue;
        };
        for d in c.dependencies() {
            if let Some(dep) = specs.iter().find(|c| c.id() == d)
                && seen.insert(dep.id())
            {
                stack.push(dep.id());
            }
        }
    }
    seen
}

/// The cargo subcommands that load the workspace before they act. Measured
/// with cargo 1.95 for `test`, `fmt` (through `cargo metadata`) and
/// `metadata`; the rest build a `Workspace` the same way.
const LOADING_VERBS: &[&str] = &[
    "build",
    "b",
    "test",
    "t",
    "check",
    "c",
    "clippy",
    "run",
    "r",
    "bench",
    "doc",
    "d",
    "fmt",
    "metadata",
    "tree",
    "fix",
    "rustc",
    "rustdoc",
    "nextest",
    "llvm-cov",
    "miri",
    "fetch",
    "update",
    "generate-lockfile",
    "package",
];

/// The flags of a cargo subcommand that take a value, for the command shown.
const VALUE_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "--manifest-path",
    "--bin",
    "--test",
    "--example",
    "--bench",
    "--features",
    "-F",
    "--target",
    "--exclude",
    "-j",
    "--jobs",
    "--profile",
    "--target-dir",
    "--color",
    "--config",
    "-Z",
    "--message-format",
];

/// One cargo command an acceptance runs: the command as shown in a refusal,
/// and the repository directory cargo looks for the manifest in (`""` is the
/// root).
#[derive(Debug, PartialEq)]
struct CargoRun {
    command: String,
    dir: String,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    /// `&&`, `||`, `;`, `|`, `&` or a line end.
    Sep,
    Open,
    Close,
}

/// The words of a shell-like acceptance. An acceptance is often prose around
/// a command, so quotes and backticks only end words; nothing is unquoted.
fn tokens(text: &str) -> Vec<Tok> {
    fn flush(word: &mut String, out: &mut Vec<Tok>) {
        if !word.is_empty() {
            out.push(Tok::Word(std::mem::take(word)));
        }
    }
    let mut out = Vec::new();
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ';' | '\n' | '&' | '|' => {
                flush(&mut word, &mut out);
                if matches!(ch, '&' | '|') && chars.peek() == Some(&ch) {
                    chars.next();
                }
                out.push(Tok::Sep);
            }
            '(' => {
                flush(&mut word, &mut out);
                out.push(Tok::Open);
            }
            ')' => {
                flush(&mut word, &mut out);
                out.push(Tok::Close);
            }
            c if c.is_whitespace() || matches!(c, '`' | '\'' | '"' | ',') => {
                flush(&mut word, &mut out);
            }
            c => word.push(c),
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A word without the sentence punctuation prose leaves on it (`test.`).
fn clean(word: &str) -> &str {
    let t = word.trim_end_matches(['.', ':']);
    if t.is_empty() { word } else { t }
}

/// `rel` from the repository directory `base`; `None` when it leaves the
/// repository or cannot be known (absolute, home or a variable).
fn join(base: &str, rel: &str) -> Option<String> {
    if rel.is_empty() || rel == "-" || rel.starts_with(['/', '~', '$', '\\']) {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for p in rel.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// The cargo command `words` (the words after `cargo`) start, run from `cwd`.
fn cargo_run(words: &[&str], cwd: Option<&str>) -> Option<CargoRun> {
    let mut dir = cwd.map(str::to_owned);
    let mut manifest: Option<&str> = None;
    let mut i = 0;
    // The toolchain and the global flags before the subcommand.
    while let Some(w) = words.get(i).map(|w| clean(w)) {
        if w.starts_with('+') {
            i += 1;
        } else if let Some(v) = w.strip_prefix("--manifest-path=") {
            manifest = Some(v);
            i += 1;
        } else if w == "--manifest-path" {
            manifest = words.get(i + 1).map(|w| clean(w));
            i += 2;
        } else if w == "-C" {
            dir = match (dir, words.get(i + 1)) {
                (Some(d), Some(to)) => join(&d, clean(to)),
                _ => None,
            };
            i += 2;
        } else if matches!(w, "-Z" | "--config" | "--color") {
            i += 2;
        } else if w.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let verb = clean(words.get(i)?);
    if !LOADING_VERBS.contains(&verb) {
        return None;
    }
    let mut shown = vec!["cargo", verb];
    let mut j = i + 1;
    // The subcommand's flags, up to the `--` that starts the arguments of
    // the program cargo runs, or the first word that is not a flag.
    while let Some(w) = words.get(j).map(|w| clean(w)) {
        if w == "--" || !w.starts_with('-') {
            break;
        }
        shown.push(w);
        if let Some(v) = w.strip_prefix("--manifest-path=") {
            manifest = Some(v);
        }
        if VALUE_FLAGS.contains(&w)
            && let Some(v) = words.get(j + 1).map(|w| clean(w))
        {
            if w == "--manifest-path" {
                manifest = Some(v);
            }
            shown.push(v);
            j += 1;
        }
        j += 1;
    }
    let mut dir = dir?;
    if let Some(m) = manifest {
        let parent = m.rsplit_once('/').map_or("", |(p, _)| p);
        if !parent.is_empty() {
            dir = join(&dir, parent)?;
        }
    }
    Some(CargoRun {
        command: shown.join(" "),
        dir,
    })
}

/// Every cargo command `acceptance` runs that loads a workspace, with the
/// directory it runs in: after a `cd`, from `--manifest-path`'s directory or
/// `-C`, else the repository root. A command in a directory that cannot be
/// known is left out.
fn cargo_runs(acceptance: &str) -> Vec<CargoRun> {
    fn segment(words: &[&str], cwd: &mut Option<String>, runs: &mut Vec<CargoRun>) {
        if words.first() == Some(&"cd") {
            *cwd = match (cwd.as_deref(), words.get(1)) {
                (Some(base), Some(to)) => join(base, clean(to)),
                // A bare `cd` goes home, out of the repository.
                _ => None,
            };
            return;
        }
        let starts: Vec<usize> = words
            .iter()
            .enumerate()
            .filter(|(_, w)| **w == "cargo")
            .map(|(i, _)| i)
            .collect();
        for (k, at) in starts.iter().enumerate() {
            let end = starts.get(k + 1).copied().unwrap_or(words.len());
            if let Some(run) = cargo_run(&words[at + 1..end], cwd.as_deref()) {
                runs.push(run);
            }
        }
    }
    let toks = tokens(acceptance);
    let mut runs = Vec::new();
    let mut cwd: Option<String> = Some(String::new());
    let mut saved: Vec<Option<String>> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    for t in &toks {
        match t {
            Tok::Word(w) => words.push(w),
            Tok::Sep | Tok::Open | Tok::Close => {
                segment(&words, &mut cwd, &mut runs);
                words.clear();
                match t {
                    Tok::Open => saved.push(cwd.clone()),
                    Tok::Close => {
                        if let Some(c) = saved.pop() {
                            cwd = c;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    segment(&words, &mut cwd, &mut runs);
    runs
}

/// The member directories a workspace names in `text`: the quoted strings of
/// the list after `members`, as `members = ["a", "b"]` or `members: ["a"]`
/// reads. Globs are kept; [`Members::from_list`] sorts them out.
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
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A workspace's members as one contract sees them, relative to the
/// workspace's directory.
#[derive(Debug, Default, PartialEq)]
struct Members {
    explicit: Vec<String>,
    /// The `<prefix>` of each `<prefix>/*` member. Other globs are not read.
    globs: Vec<String>,
    exclude: Vec<String>,
}

impl Members {
    fn from_list(list: impl IntoIterator<Item = String>, exclude: Vec<String>) -> Self {
        let mut m = Members {
            exclude,
            ..Members::default()
        };
        for item in list {
            let item = item.trim_start_matches("./").trim_end_matches('/');
            if item.is_empty() || item == "." {
                continue;
            }
            match item.strip_suffix("/*") {
                Some(prefix) if !prefix.contains(['*', '?', '[']) => {
                    m.globs.push(prefix.to_owned());
                }
                _ if item.contains(['*', '?', '[']) => {}
                _ => m.explicit.push(item.to_owned()),
            }
        }
        m
    }

    fn is_empty(&self) -> bool {
        self.explicit.is_empty() && self.globs.is_empty()
    }
}

/// The members of `manifest` as the repository holds it; `None` when the file
/// is absent, does not parse or has no `[workspace]`.
fn repo_members(manifest: &str, repo: &dyn RepoView) -> Option<Members> {
    let table = repo.read(manifest)?.parse::<toml::Table>().ok()?;
    let ws = table.get("workspace")?;
    let strings = |key: &str| -> Vec<String> {
        ws.get(key)
            .and_then(|m| m.as_array())
            .map(|m| {
                m.iter()
                    .filter_map(|x| x.as_str())
                    .map(|x| x.trim_start_matches("./").trim_end_matches('/').to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(Members::from_list(strings("members"), strings("exclude")))
}

/// Words that end a prose member list: what follows them is not a member
/// the manifest lists now ("members wcl-core, not wcl", "members wcl-core
/// until c2 adds wcl").
const PROSE_LIST_ENDS: &[&str] = &[
    "not",
    "except",
    "excluding",
    "until",
    "without",
    "but",
    "later",
    "after",
    "then",
    "once",
    "when",
    "unless",
];

/// The member directories an instruction names in prose after the word
/// `member` or `members`, as planners write it: "the workspace root
/// Cargo.toml with members wcl-core and wcl", "member crates `a`, `b`". The
/// list is read to the end of its sentence (a period before a space, a
/// semicolon, a parenthesis or a line break) or to a word of
/// [`PROSE_LIST_ENDS`], and only a word that `is_member_dir` accepts counts.
/// `is_member_dir` is the guard against prose: a word is a member only when
/// the plan writes, or the repository holds, a `Cargo.toml` in that
/// directory, so "members listed below" names none.
fn prose_members(text: &str, is_member_dir: &dyn Fn(&str) -> bool) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut found: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("member") {
        let start = from + at;
        from = start + "member".len();
        // A whole word: `member` or `members`, not `remember` or `membership`.
        let before_ok = lower[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        // `lower` is the ASCII lowercase of `text`, so byte offsets agree.
        if lower[from..].starts_with('s') {
            from += 1;
        }
        let rest = &text[from..];
        if !before_ok || rest.chars().next().is_some_and(char::is_alphanumeric) {
            continue;
        }
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            let next = rest[i + c.len_utf8()..].chars().next();
            let sentence_end = c == '.' && next.is_none_or(char::is_whitespace);
            if sentence_end || matches!(c, ';' | '(' | '\n' | '\r') {
                end = i;
                break;
            }
        }
        for word in rest[..end].split(|c: char| c.is_whitespace() || c == ',') {
            let word = word
                .trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | ':' | '[' | ']' | '.'))
                .trim_start_matches("./")
                .trim_end_matches('/');
            if PROSE_LIST_ENDS.contains(&word.to_ascii_lowercase().as_str()) {
                break;
            }
            if !word.is_empty() && is_member_dir(word) && !found.iter().any(|f| f == word) {
                found.push(word.to_owned());
            }
        }
    }
    found
}

/// The members `writer`'s instruction lists for `manifest`. Only for the root
/// manifest: a nested one the plan writes is a member's, and the
/// instruction's member list is not about it.
///
/// A `members = [...]` list is read first. A list stated in prose ("with
/// members wcl-core and wcl") is read too, but only for words that are
/// member directories of this plan or repository: a plan that wrote its list
/// in prose was read as listing no member, and its root contract then failed
/// on a later contract's missing member manifest.
fn instruction_members<C: PlanContract>(
    specs: &[C],
    writer: &C,
    manifest: &str,
    repo: &dyn RepoView,
) -> Members {
    if manifest != "Cargo.toml" {
        return Members::default();
    }
    let listed = members_in(writer.instruction());
    if !listed.is_empty() {
        return Members::from_list(listed, Vec::new());
    }
    let written = writes_of(specs);
    let is_member_dir = |dir: &str| {
        let m = format!("{dir}/Cargo.toml");
        written.contains_key(m.as_str()) || repo.exists(&m)
    };
    Members::from_list(
        prose_members(writer.instruction(), &is_member_dir),
        Vec::new(),
    )
}

/// The members of `manifest` as a contract that can count on `supplied` sees
/// them, or `None` when the manifest is neither in the repository nor written
/// by a contract of `supplied`. The last writer's instruction wins, then the
/// repository's file.
fn seen_members<C: PlanContract>(
    specs: &[C],
    supplied: &BTreeSet<&str>,
    manifest: &str,
    repo: &dyn RepoView,
) -> Option<Members> {
    let writers: Vec<&C> = specs
        .iter()
        .filter(|s| supplied.contains(s.id()))
        .filter(|s| s.write_paths().iter().any(|w| norm(w) == manifest))
        .collect();
    let from_repo = || repo_members(manifest, repo);
    let Some(first) = writers.first().copied() else {
        return repo
            .exists(manifest)
            .then(|| from_repo().unwrap_or_default());
    };
    // The last writer: one every other writer here comes before. The
    // contract itself, when it writes the file, since the rest of `supplied`
    // are its dependencies.
    let last = writers
        .iter()
        .find(|w| {
            let before = ancestors(specs, w.id());
            writers
                .iter()
                .all(|o| o.id() == w.id() || before.contains(o.id()))
        })
        .copied()
        .unwrap_or(first);
    let listed = instruction_members(specs, last, manifest, repo);
    Some(if listed.is_empty() {
        from_repo().unwrap_or_default()
    } else {
        Members {
            exclude: from_repo().map(|m| m.exclude).unwrap_or_default(),
            ..listed
        }
    })
}

/// One reason a contract's cargo acceptance cannot pass, and the files it is
/// about: the missing file first, then the files its remedy writes with it.
#[derive(Debug)]
struct Finding {
    paths: Vec<String>,
    reason: String,
}

/// The directory of a `Cargo.toml` path, `""` for the root.
fn dir_of(manifest: &str) -> &str {
    manifest
        .strip_suffix("Cargo.toml")
        .unwrap_or("")
        .trim_end_matches('/')
}

fn within(dir: &str, run_dir: &str) -> bool {
    dir.is_empty() || run_dir == dir || run_dir.starts_with(&format!("{dir}/"))
}

/// Why `c`'s acceptance cannot pass in the workspace it runs in: cargo
/// ([`cargo_findings`]), Go and Maven ([`module_findings`]).
///
/// Cargo loads the manifest of every member of the workspace before it runs
/// any command: with `--workspace`, with `-p <package>`, with
/// `--manifest-path <member>/Cargo.toml` and from a member's directory alike
/// (measured with cargo 1.95: "failed to load manifest for workspace
/// member"). A member manifest with no target is refused the same way ("no
/// targets specified in the manifest"). So every member the workspace lists
/// must have its manifest and a Rust source in the repository, or written by
/// `c` or a contract it depends on, and a command run at the workspace's root
/// needs the workspace manifest itself.
fn workspace_findings<C: PlanContract>(specs: &[C], c: &C, repo: &dyn RepoView) -> Vec<Finding> {
    let mut found = cargo_findings(specs, c, repo);
    for f in module_findings(specs, c, repo) {
        if !found.iter().any(|g| g.paths[0] == f.paths[0]) {
            found.push(f);
        }
    }
    found
}

/// A Go or Maven command of an acceptance, with the directory it runs in and
/// the manifest it loads its members from.
#[derive(Debug)]
struct ToolRun {
    command: String,
    tool: ModuleTool,
    dir: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleTool {
    /// A `go` command that loads packages: it reads the nearest `go.work`
    /// above its directory, and every module the file `use`s.
    Go,
    /// `mvn` or `./mvnw`: the `pom.xml` of its directory (or `-f`) and every
    /// module that pom lists.
    Maven,
}

/// `go` subcommands that load the workspace.
const GO_LOADING_VERBS: &[&str] = &["build", "test", "vet", "run", "list", "install", "generate"];

/// The Go and Maven commands `acceptance` runs, with their directories, as
/// [`cargo_runs`] reads cargo's. A `go` command after `GOWORK=off` loads no
/// workspace and is left out; so is a command in a directory that cannot be
/// known.
fn module_runs(acceptance: &str) -> Vec<ToolRun> {
    fn segment(words: &[&str], cwd: &mut Option<String>, runs: &mut Vec<ToolRun>) {
        if words.first() == Some(&"cd") {
            *cwd = match (cwd.as_deref(), words.get(1)) {
                (Some(base), Some(to)) => join(base, clean(to)),
                _ => None,
            };
            return;
        }
        let Some(base) = cwd.clone() else {
            return;
        };
        let no_work = words.iter().any(|w| w.starts_with("GOWORK="));
        for (i, w) in words.iter().enumerate() {
            let w = clean(w);
            if w == "go" && !no_work {
                let mut dir = Some(base.clone());
                let mut j = i + 1;
                while let Some(a) = words.get(j).map(|a| clean(a)) {
                    if a == "-C" {
                        dir = match (dir, words.get(j + 1)) {
                            (Some(d), Some(to)) => join(&d, clean(to)),
                            _ => None,
                        };
                        j += 2;
                    } else {
                        break;
                    }
                }
                let verb = words.get(j).map(|v| clean(v)).unwrap_or_default();
                if let Some(dir) = dir.filter(|_| GO_LOADING_VERBS.contains(&verb)) {
                    runs.push(ToolRun {
                        command: format!("go {verb}"),
                        tool: ModuleTool::Go,
                        dir,
                    });
                }
            } else if w == "mvn" || w == "mvnw" || w == "./mvnw" {
                let mut dir = Some(base.clone());
                let mut j = i + 1;
                while let Some(a) = words.get(j).map(|a| clean(a)) {
                    if a == "-f" || a == "--file" {
                        dir = match (dir, words.get(j + 1)) {
                            (Some(d), Some(to)) => {
                                let to = clean(to);
                                let parent = if to.ends_with(".xml") {
                                    to.rsplit_once('/').map_or("", |(p, _)| p)
                                } else {
                                    to
                                };
                                if parent.is_empty() {
                                    Some(d)
                                } else {
                                    join(&d, parent)
                                }
                            }
                            _ => None,
                        };
                        j += 2;
                        continue;
                    }
                    j += 1;
                    if !a.starts_with('-') {
                        break;
                    }
                }
                if let Some(dir) = dir {
                    runs.push(ToolRun {
                        command: "mvn".to_owned(),
                        tool: ModuleTool::Maven,
                        dir,
                    });
                }
            }
        }
    }
    let mut runs = Vec::new();
    let mut cwd: Option<String> = Some(String::new());
    let mut saved: Vec<Option<String>> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    let toks = tokens(acceptance);
    for t in &toks {
        match t {
            Tok::Word(w) => words.push(w),
            Tok::Sep | Tok::Open | Tok::Close => {
                segment(&words, &mut cwd, &mut runs);
                words.clear();
                match t {
                    Tok::Open => saved.push(cwd.clone()),
                    Tok::Close => {
                        if let Some(c) = saved.pop() {
                            cwd = c;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    segment(&words, &mut cwd, &mut runs);
    runs
}

/// The module directories a `go.work` file `use`s: `use ./a` and the lines of
/// a `use ( ... )` block. A directory outside the repository is left out.
fn go_work_uses(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else if !line.is_empty() {
                out.push(line.trim_matches('"').to_owned());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("use") {
            let rest = rest.trim();
            if rest.starts_with('(') {
                in_block = true;
                let inner = rest.trim_start_matches('(').trim();
                if let Some(one) = inner.strip_suffix(')') {
                    in_block = false;
                    if !one.trim().is_empty() {
                        out.push(one.trim().trim_matches('"').to_owned());
                    }
                }
            } else if !rest.is_empty() {
                out.push(rest.trim_matches('"').to_owned());
            }
        }
    }
    out
}

/// The modules a `pom.xml` lists in its `<modules>`, comments and profiles
/// left out: a profile's modules load only when it is active.
fn maven_modules(text: &str) -> Vec<String> {
    let mut t = text.to_owned();
    for (open, close) in [("<!--", "-->"), ("<profiles>", "</profiles>")] {
        while let Some(a) = t.find(open) {
            match t[a..].find(close) {
                Some(b) => t.replace_range(a..a + b + close.len(), ""),
                None => {
                    t.truncate(a);
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut rest = t.as_str();
    while let Some(a) = rest.find("<module>") {
        let after = &rest[a + "<module>".len()..];
        let Some(b) = after.find("</module>") else {
            break;
        };
        let m = after[..b].trim();
        if !m.is_empty() {
            out.push(m.to_owned());
        }
        rest = &after[b..];
    }
    out
}

/// Why `c`'s Go or Maven acceptance cannot pass: a member its workspace or
/// reactor lists whose manifest is neither in the repository nor written by
/// `c` or a contract it depends on.
///
/// - **Go**: every `go` command that loads packages reads the nearest
///   `go.work` above its directory and loads every module it `use`s; a used
///   directory with no `go.mod` makes the command fail ("cannot load module
///   ... listed in go.work file").
/// - **Maven**: `mvn` builds the reactor of the `pom.xml` it runs on; a listed
///   `<module>` with no `pom.xml` makes the build fail ("Child module ... does
///   not exist").
///
/// Only a manifest the repository holds, and that `c` and its dependencies do
/// not rewrite, is read: a manifest a contract writes has content the plan
/// does not show, so it is not judged. A finding is only made when the load
/// is certain; every other case is left unchecked. Measured not to load a
/// missing member, and so not checked: npm (a listed workspace directory that
/// is missing is skipped), uv. Not checked because the outcome depends on the
/// version: Gradle (a project in `settings.gradle` with no directory is a
/// deprecation warning in Gradle 8 and an error later), pnpm.
fn module_findings<C: PlanContract>(specs: &[C], c: &C, repo: &dyn RepoView) -> Vec<Finding> {
    let runs = module_runs(c.acceptance());
    if runs.is_empty() {
        return Vec::new();
    }
    let written = writes_of(specs);
    let mut supplied: BTreeSet<&str> = ancestors(specs, c.id());
    supplied.insert(c.id());
    let by_supplied = |path: &str| {
        written
            .get(path)
            .is_some_and(|ids| ids.iter().any(|id| supplied.contains(id)))
    };
    let reachable = |path: &str| repo.exists(path) || by_supplied(path);
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
                c.id()
            ),
        }
    };
    let at = |dir: &str| -> String {
        if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        }
    };
    let mut found: Vec<Finding> = Vec::new();
    for run in &runs {
        // The manifest that lists the members, and the members.
        let (manifest, members, member_file) = match run.tool {
            ModuleTool::Go => {
                let mut dir = Some(run.dir.clone());
                let mut work = None;
                while let Some(d) = dir {
                    let p = format!("{}go.work", at(&d));
                    if repo.exists(&p) || written.contains_key(p.as_str()) {
                        work = Some((p, d.clone()));
                        break;
                    }
                    dir = (!d.is_empty())
                        .then(|| d.rsplit_once('/').map_or("", |(p, _)| p).to_owned());
                }
                let Some((manifest, wdir)) = work else {
                    continue;
                };
                if by_supplied(&manifest) {
                    continue;
                }
                let Some(text) = repo.read(&manifest) else {
                    continue;
                };
                let members: Vec<String> = go_work_uses(&text)
                    .into_iter()
                    .filter_map(|u| join(&wdir, &u))
                    .collect();
                (manifest, members, "go.mod")
            }
            ModuleTool::Maven => {
                let manifest = format!("{}pom.xml", at(&run.dir));
                if by_supplied(&manifest) {
                    continue;
                }
                let Some(text) = repo.read(&manifest) else {
                    continue;
                };
                let members: Vec<String> = maven_modules(&text)
                    .into_iter()
                    .filter_map(|m| join(&run.dir, &m))
                    .collect();
                (manifest, members, "pom.xml")
            }
        };
        for member in members {
            let member_manifest = if member.ends_with(".xml") {
                member.clone()
            } else {
                format!("{}{member_file}", at(&member))
            };
            if reachable(&member_manifest) || found.iter().any(|f| f.paths[0] == member_manifest) {
                continue;
            }
            let (tool, load) = match run.tool {
                ModuleTool::Go => (
                    "the go command",
                    "loads every module its `go.work` uses before it runs",
                ),
                ModuleTool::Maven => (
                    "maven",
                    "builds every module its `pom.xml` lists, and stops on a missing one",
                ),
            };
            found.push(Finding {
                paths: vec![member_manifest.clone()],
                reason: format!(
                    "contract `{id}` acceptance runs `{command}`, and {tool} {load}. \
                     `{manifest}` lists `{member}`, but `{member_manifest}` does not exist and is \
                     written {by}, so the command fails when `{id}` runs. Make `{id}` also write \
                     `{member_manifest}`, or make `{id}` depend on the contract that writes it",
                    id = c.id(),
                    command = run.command,
                    by = writers(&member_manifest),
                ),
            });
        }
    }
    found
}

/// The cargo half of [`workspace_findings`], as its documentation describes.
fn cargo_findings<C: PlanContract>(specs: &[C], c: &C, repo: &dyn RepoView) -> Vec<Finding> {
    let runs = cargo_runs(c.acceptance());
    if runs.is_empty() {
        return Vec::new();
    }
    let written = writes_of(specs);
    let mut manifests: BTreeSet<&str> = written
        .keys()
        .copied()
        .filter(|p| *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
        .collect();
    if repo.exists("Cargo.toml") {
        manifests.insert("Cargo.toml");
    }
    let everyone: BTreeSet<&str> = specs.iter().map(|s| s.id()).collect();
    // The workspaces: a manifest some writer's instruction or the repository
    // gives members. Nearest first, as cargo searches up from a directory.
    let mut workspaces: Vec<&str> = manifests
        .into_iter()
        .filter(|m| seen_members(specs, &everyone, m, repo).is_some_and(|w| !w.is_empty()))
        .collect();
    workspaces.sort_by_key(|m| std::cmp::Reverse(m.matches('/').count()));
    if workspaces.is_empty() {
        return Vec::new();
    }
    let mut supplied: BTreeSet<&str> = ancestors(specs, c.id());
    supplied.insert(c.id());
    let by_supplied = |ids: &Vec<&str>| ids.iter().any(|id| supplied.contains(id));
    let reachable = |path: &str| repo.exists(path) || written.get(path).is_some_and(&by_supplied);
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
                c.id()
            ),
        }
    };
    // A writer of `path` that `c` may depend on: one that does not depend on
    // `c`, so the edge makes no cycle.
    let free_writer = |path: &str| -> Option<&str> {
        written
            .get(path)?
            .iter()
            .copied()
            .find(|w| !ancestors(specs, w).contains(c.id()))
    };
    let mut found: Vec<Finding> = Vec::new();
    let mut push = |f: Finding| {
        if !found.iter().any(|g| g.paths[0] == f.paths[0]) {
            found.push(f);
        }
    };
    for run in &runs {
        for manifest in workspaces.iter().copied() {
            let dir = dir_of(manifest);
            if !within(dir, &run.dir) {
                continue;
            }
            let Some(members) = seen_members(specs, &supplied, manifest, repo) else {
                // Not in the repository and not written in time. From a
                // member's own directory cargo finds that member's manifest
                // alone; at the workspace's root it finds none.
                if run.dir == dir {
                    let depend = free_writer(manifest)
                        .map(|w| format!(" or depend on `{w}`, which writes it"))
                        .unwrap_or_default();
                    push(Finding {
                        paths: vec![manifest.to_owned()],
                        reason: format!(
                            "contract `{id}` acceptance runs `{command}` at {at}, but the \
                             workspace manifest `{manifest}` is not in the repository and is \
                             written {by}, so cargo finds no manifest when `{id}` runs. Make \
                             `{id}` write `{manifest}`{depend}",
                            id = c.id(),
                            command = run.command,
                            at = if dir.is_empty() {
                                "the repository root".to_owned()
                            } else {
                                format!("`{dir}`")
                            },
                            by = writers(manifest),
                        ),
                    });
                    break;
                }
                continue;
            };
            if members.is_empty() {
                // A package's manifest: cargo searches further up.
                continue;
            }
            let base = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let mut dirs: Vec<String> = members
                .explicit
                .iter()
                .filter(|m| !members.exclude.contains(m))
                .cloned()
                .collect();
            // A `<prefix>/*` member is every directory under `<prefix>`,
            // and a directory a contract writes into exists in its worktree.
            for g in &members.globs {
                let prefix = format!("{base}{g}/");
                for (path, ids) in &written {
                    if !by_supplied(ids) {
                        continue;
                    }
                    if let Some(rest) = path.strip_prefix(&prefix)
                        && let Some((name, _)) = rest.split_once('/')
                    {
                        let member = format!("{g}/{name}");
                        if !members.exclude.contains(&member) && !dirs.contains(&member) {
                            dirs.push(member);
                        }
                    }
                }
            }
            for member in dirs {
                let member_manifest = format!("{base}{member}/Cargo.toml");
                let src = format!("{base}{member}/src");
                if !reachable(&member_manifest) {
                    let depend = free_writer(&member_manifest)
                        .map(|w| format!("; or make `{}` depend on `{w}`", c.id()))
                        .unwrap_or_default();
                    push(Finding {
                        paths: vec![
                            member_manifest.clone(),
                            format!("{src}/lib.rs"),
                            format!("{src}/main.rs"),
                        ],
                        reason: format!(
                            "contract `{id}` acceptance runs `{command}`, and cargo loads the \
                             manifest of every member of the workspace `{manifest}` before it \
                             runs any command, with `-p`, with `--manifest-path` or from a \
                             member's directory alike. `{manifest}` lists member `{member}`, \
                             but `{member_manifest}` does not exist and is written {by}, so \
                             cargo fails on the missing manifest when `{id}` runs. Fix it in one \
                             of these ways: make `{id}` also write `{member_manifest}` (a \
                             `[package]` with name, version and edition) and a source file \
                             `{src}/lib.rs` or `{src}/main.rs`, which a later contract may \
                             fill in; or have `{manifest}` list only the members that exist or \
                             that `{id}` and the contracts it depends on write, and make the \
                             contract that writes `{member_manifest}` also write `{manifest}` \
                             to add `{member}` to its members{depend}",
                            id = c.id(),
                            command = run.command,
                            by = writers(&member_manifest),
                        ),
                    });
                    continue;
                }
                if repo.exists(&member_manifest) {
                    continue;
                }
                let has_source = repo.exists(&format!("{src}/lib.rs"))
                    || repo.exists(&format!("{src}/main.rs"))
                    || written.iter().any(|(p, ids)| {
                        p.starts_with(&format!("{base}{member}/"))
                            && p.ends_with(".rs")
                            && by_supplied(ids)
                    });
                if !has_source {
                    push(Finding {
                        paths: vec![format!("{src}/lib.rs"), format!("{src}/main.rs")],
                        reason: format!(
                            "contract `{id}` acceptance runs `{command}`, and cargo loads every \
                             member of the workspace `{manifest}` first. Member `{member}` gets \
                             its manifest `{member_manifest}`, but no Rust source under \
                             `{base}{member}/` is in the repository or written by `{id}` or a \
                             contract it depends on, and cargo refuses a manifest with no \
                             targets (\"no targets specified in the manifest\"). Make the \
                             contract that writes `{member_manifest}` also write \
                             `{src}/lib.rs` or `{src}/main.rs` (a minimal one is enough)",
                            id = c.id(),
                            command = run.command,
                        ),
                    });
                }
            }
            break;
        }
    }
    found
}

/// The first contract whose cargo acceptance cannot pass in its workspace.
fn workspace_refusal(specs: &[ContractSpec], repo: &dyn RepoView) -> Option<String> {
    specs
        .iter()
        .find_map(|c| workspace_findings(specs, c, repo).into_iter().next())
        .map(|f| f.reason)
}

/// The hold for a worker of `id` that asked for one of the files `named`,
/// when that file is one the contract's cargo acceptance cannot pass without
/// and only a contract that runs later writes it. Answering "do not edit it"
/// would leave the worker at a check that cannot pass, and a grant would
/// break disjoint writes: the plan's shape is wrong, so the contract is held
/// for a re-plan. A file no open contract writes is left to the plan-shape
/// hold of [`crate::shortfall::decide_in_plan`]; a file a finished contract
/// wrote is in the attempt's base and never asked for here.
pub(crate) fn member_hold(
    open: &[OpenContract],
    id: &str,
    named: &[&str],
    repo: &dyn RepoView,
) -> Option<String> {
    let c = open.iter().find(|c| c.contract == id)?;
    let written = writes_of(open);
    let (finding, path) = workspace_findings(open, c, repo)
        .into_iter()
        .find_map(|f| {
            let path = named
                .iter()
                .map(|n| norm(n))
                .find(|n| f.paths.iter().any(|p| p == n) && written.contains_key(n))?;
            Some((f, path.to_owned()))
        })?;
    Some(format!(
        "a worker of `{id}` needs `{path}`, which `{id}` may not write and its acceptance check \
         (`{acceptance}`) cannot pass without. That is a plan-shape error, not a missing \
         permission: a grant would break disjoint writes, and the check cannot pass as \
         planned. Re-plan, then run again. The plan check reads: {reason}",
        acceptance = c.acceptance.trim(),
        reason = finding.reason,
    ))
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

    /// A correct Python or Ruby project's abstract base class raises
    /// `NotImplementedError` for its subclasses to override. The instruction
    /// names the module so the worker can read the base, and the contract
    /// writes only the new subclass: the plan is accepted. A module-level
    /// scaffold beside it is still refused.
    #[test]
    fn a_named_abstract_base_class_is_not_an_unwritten_scaffold() {
        let repo = Fake::with(&[
            (
                "shapes/base.py",
                "from abc import ABC, abstractmethod\n\n\nclass Shape(ABC):\n    @abstractmethod\n    def area(self) -> float:\n        raise NotImplementedError\n",
            ),
            (
                "shapes/plain.py",
                "class Shape:\n    def area(self):\n        raise NotImplementedError\n\n\nclass Square(Shape):\n    def area(self):\n        return 4.0\n",
            ),
            (
                "lib/shape.rb",
                "class Shape\n  def area\n    raise NotImplementedError, \"subclasses override area\"\n  end\nend\n",
            ),
            (
                "shapes/todo.py",
                "class Shape(ABC):\n    pass\n\n\ndef circle_area(r):\n    raise NotImplementedError\n",
            ),
        ]);
        let specs = [spec(
            "c1",
            "add a Circle subclass of Shape",
            "python3 -m pytest",
            &["shapes/circle.py"],
            &[],
        )];
        for named in ["shapes/base.py", "shapes/plain.py", "lib/shape.rb"] {
            assert!(
                refusal(&specs, &[named.into()], &repo).is_none(),
                "{named} was refused"
            );
        }
        let why = reason(refusal(&specs, &["shapes/todo.py".into()], &repo));
        assert!(why.contains("shapes/todo.py"), "{why}");
    }

    /// Shaped like `crates/kgraf-host/src/scoped_check.rs` as of
    /// 2026-10-02 (self-repo): its only `todo!()` is prose in string
    /// literals. The macro is also named in a raw string and in comments.
    const PROSE_ONLY: &str = r###"pub fn brief() -> &'static str {
    "Acceptance is advisory. `todo!()` panics or failing tests \
     in other contracts' unfinished code are not yours to fix."
}

pub fn raw() -> &'static str {
    r#"unimplemented!() in a "raw" string"#
}

// todo!() in a comment
/* unimplemented!() /* nested */ still a comment */
pub fn f() {}
"###;

    /// The instruction named that file, no contract wrote it, and every plan
    /// was refused for leaving a placeholder unwritten. A placeholder in a
    /// string, raw string or comment is text, not a body.
    #[test]
    fn a_placeholder_named_only_in_strings_and_comments_is_not_refused() {
        let real = format!("{PROSE_ONLY}pub fn g() -> u8 {{\n    todo!()\n}}\n");
        let repo = Fake::with(&[
            ("crates/h/src/scoped_check.rs", PROSE_ONLY),
            ("crates/h/src/real.rs", real.as_str()),
            (
                "py/doc.py",
                "def f():\n    \"\"\"raise NotImplementedError until done.\"\"\"\n    return 1\n",
            ),
        ]);
        let specs = [spec("c1", "x", "cargo test -p z", &["z/src/lib.rs"], &[])];
        for text_only in ["crates/h/src/scoped_check.rs", "py/doc.py"] {
            let got = refusal(&specs, &[text_only.into()], &repo);
            assert!(got.is_none(), "{text_only}: {got:?}");
        }
        // The same text next to a real `todo!()` body is still refused.
        let why = reason(refusal(&specs, &["crates/h/src/real.rs".into()], &repo));
        assert!(why.contains("`crates/h/src/real.rs`"), "{why}");
    }

    /// Round evidence (greenfield, first plan): c1 writes `Cargo.toml` listing
    /// members `wcl-core` and `wcl` and accepts on `cargo test --workspace`;
    /// `wcl` is written only by c2, which c1 does not depend on. The attempt
    /// could not pass.
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

    /// Round 8 evidence (greenfield, the plan accepted after the first
    /// refusal, verbatim but for the long instruction bodies): c1-core writes
    /// the root manifest with members `wcl-core` and `wcl` and accepts on
    /// `cargo test -p wcl-core`; `wcl/Cargo.toml` belongs to c2-bin, which
    /// depends on c1-core. Cargo loads `wcl/Cargo.toml` for `-p wcl-core`
    /// too, so neither attempt of c1-core could pass.
    fn round8_second_plan() -> [ContractSpec; 2] {
        [
            spec(
                "c1-core",
                "Create the workspace root and `wcl-core` library.\n1. Write `Cargo.toml` as:\n[workspace]\nmembers = [\"wcl-core\", \"wcl\"]\n\n2. Write `wcl-core/Cargo.toml` with package name `wcl-core`, version `0.1.0`, and `[lib] path = \"src/lib.rs\"`.\n3. Write `wcl-core/src/lib.rs`.",
                "cargo test -p wcl-core && echo SUCCESS",
                &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"],
                &[],
            ),
            spec(
                "c2-bin",
                "Create the `wcl` binary crate and verify the full workspace.\n1. Write `wcl/Cargo.toml` with package name `wcl`.\n2. Write `wcl/src/main.rs`.",
                "cargo test --workspace && echo SUCCESS",
                &["wcl/Cargo.toml", "wcl/src/main.rs"],
                &["c1-core"],
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
        // c2 does not depend on c1, so c1 may depend on c2.
        assert!(why.contains("make `c1` depend on `c2`"), "{why}");
    }

    /// The round 8 fault: the `-p` form loads every member as well.
    #[test]
    fn the_package_form_with_a_member_a_later_contract_writes_is_refused() {
        let why = reason(refusal(&round8_second_plan(), &[], &Fake::default()));
        assert!(why.contains("`c1-core`"), "{why}");
        assert!(why.contains("`cargo test -p wcl-core`"), "{why}");
        assert!(why.contains("`wcl/Cargo.toml`"), "{why}");
        assert!(
            why.contains("only by contract `c2-bin`, which `c1-core` does not depend on"),
            "{why}"
        );
        // The remedies are the measured ones; `-p` is not one of them.
        assert!(
            why.contains("also write `wcl/Cargo.toml`")
                && why.contains("`wcl/src/lib.rs` or `wcl/src/main.rs`"),
            "{why}"
        );
        assert!(why.contains("to add `wcl` to its members"), "{why}");
        assert!(!why.contains("cargo test -p <package>"), "{why}");
        // c2-bin depends on c1-core: the reverse edge would be a cycle, so it
        // is not offered.
        assert!(!why.contains("depend on `c2-bin`"), "{why}");
    }

    /// Every form cargo runs in the workspace loads the members: measured
    /// with cargo 1.95 for `-p`, `--manifest-path`, `cd <member>`, a bare
    /// command at the root and `cargo fmt` (through `cargo metadata`).
    #[test]
    fn every_cargo_form_in_the_workspace_is_refused() {
        for acceptance in [
            "cargo test -p wcl-core",
            "cargo test --manifest-path wcl-core/Cargo.toml",
            "cargo test --manifest-path=./wcl-core/Cargo.toml",
            "cd wcl-core && cargo test",
            "(cd wcl-core; cargo build --offline)",
            "cargo test",
            "Run cargo test. It must pass.",
            "cargo +stable --offline check -p wcl-core",
            "cargo clippy -p wcl-core -- -D warnings",
            "cargo fmt --all -- --check",
            "cargo build -p wcl-core 2>&1 | tail -n 5",
        ] {
            let mut specs = round8_second_plan();
            specs[0].acceptance = acceptance.into();
            let why = reason(refusal(&specs, &[], &Fake::default()));
            assert!(why.contains("`wcl/Cargo.toml`"), "{acceptance}: {why}");
            assert!(why.contains("`c1-core`"), "{acceptance}: {why}");
        }
        // Not cargo in this workspace: nothing to refuse.
        for acceptance in [
            "rustc --test wcl-core/src/lib.rs && ./lib",
            "cargo new wcl",
            "cargo --version",
            "the cargo is loaded",
            "cd /tmp/elsewhere && cargo test",
            "cd .. && cargo test",
        ] {
            let mut specs = round8_second_plan();
            specs[0].acceptance = acceptance.into();
            assert!(
                refusal(&specs, &[], &Fake::default()).is_none(),
                "{acceptance}: {:?}",
                refusal(&specs, &[], &Fake::default())
            );
        }
    }

    /// The two remedies the refusal names, as a planner would write them.
    /// Both are accepted, and both build with cargo
    /// (`the_check_agrees_with_cargo`).
    fn remedy_stub_members() -> [ContractSpec; 2] {
        let [mut c1, mut c2] = round8_second_plan();
        c1.writes.extend(
            ["wcl/Cargo.toml", "wcl/src/main.rs"]
                .iter()
                .map(|p| WriteTarget {
                    path: (*p).into(),
                    node: None,
                }),
        );
        c2.instruction = "Fill in the `wcl` binary crate.".into();
        [c1, c2]
    }

    fn remedy_members_add_themselves() -> [ContractSpec; 2] {
        let [mut c1, mut c2] = round8_second_plan();
        c1.instruction =
            "Write `Cargo.toml` as:\n[workspace]\nmembers = [\"wcl-core\"]\n\nand the `wcl-core` crate."
                .into();
        c2.instruction =
            "Write the `wcl` crate and add it to the workspace: `Cargo.toml` with members = [\"wcl-core\", \"wcl\"]."
                .into();
        c2.writes.push(WriteTarget {
            path: "Cargo.toml".into(),
            node: None,
        });
        [c1, c2]
    }

    #[test]
    fn the_remedies_the_refusal_names_are_accepted() {
        let repo = Fake::default();
        for (name, specs) in [
            ("stub members", remedy_stub_members()),
            ("members add themselves", remedy_members_add_themselves()),
        ] {
            let got = refusal(&specs, &[], &repo);
            assert!(got.is_none(), "{name}: {got:?}");
        }
    }

    /// A member manifest with no Rust source is refused by cargo ("no targets
    /// specified in the manifest"), measured with cargo 1.95.
    #[test]
    fn a_member_manifest_with_no_source_in_time_is_refused() {
        let [mut c1, c2] = round8_second_plan();
        c1.writes.push(WriteTarget {
            path: "wcl/Cargo.toml".into(),
            node: None,
        });
        let why = reason(refusal(&[c1, c2], &[], &Fake::default()));
        assert!(
            why.contains("no targets specified in the manifest"),
            "{why}"
        );
        assert!(
            why.contains("`wcl/src/lib.rs` or `wcl/src/main.rs`"),
            "{why}"
        );
    }

    #[test]
    fn a_member_written_by_nobody_is_named_as_such() {
        let repo = Fake::default();
        let mut specs = greenfield(&[], &[]);
        specs[1].writes.clear();
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("written by no contract"), "{why}");
        assert!(why.contains("`wcl/Cargo.toml`"), "{why}");
        assert!(!why.contains("depend on `"), "{why}");
    }

    /// Round 10 greenfield, the accepted plan verbatim: the root contract's
    /// instruction states the member list in prose. The check read no
    /// member, accepted the plan, and `c1` failed its first attempt on
    /// "failed to load manifest for workspace member `wcl`"; its retry
    /// dropped `wcl` from the members, which `c2` could not add back (the
    /// root manifest is not in its writes).
    fn round10_plan() -> [ContractSpec; 2] {
        [
            spec(
                "c1",
                "Create the workspace root Cargo.toml with members wcl-core and wcl. Create the \
                 wcl-core library crate directory structure and files (wcl-core/Cargo.toml with \
                 no dependencies, wcl-core/src/lib.rs). In wcl-core/src/lib.rs, define pub struct \
                 Counts { pub lines: usize, pub words: usize, pub bytes: usize } and pub fn \
                 count(text: &str) -> Counts.",
                "Run cargo test -p wcl-core from the repository root and confirm it exits with \
                 status 0 and reports all tests passed.",
                &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"],
                &[],
            ),
            spec(
                "c2",
                "Create the wcl binary crate directory structure and files (wcl/Cargo.toml \
                 depending on wcl-core = { path = \"../wcl-core\" }, wcl/src/main.rs). Verify \
                 the full workspace builds and tests pass.",
                "Run cargo test --workspace from the repository root and confirm it exits with \
                 status 0.",
                &["wcl/Cargo.toml", "wcl/src/main.rs"],
                &["c1"],
            ),
        ]
    }

    #[test]
    fn a_member_list_in_prose_is_read_and_refused_like_a_toml_list() {
        let why = reason(refusal(&round10_plan(), &[], &Fake::default()));
        assert!(why.contains("contract `c1`"), "{why}");
        assert!(why.contains("lists member `wcl`"), "{why}");
        assert!(why.contains("`wcl/Cargo.toml`"), "{why}");
        assert!(why.contains("only by contract `c2`"), "{why}");
        // The remedy that makes `c1` write the member's stub is accepted.
        let [mut c1, c2] = round10_plan();
        c1.writes.extend(
            ["wcl/Cargo.toml", "wcl/src/main.rs"]
                .iter()
                .map(|p| WriteTarget {
                    path: (*p).into(),
                    node: None,
                }),
        );
        let ok = [c1, c2];
        assert!(refusal(&ok, &[], &Fake::default()).is_none());
    }

    #[test]
    fn prose_members_count_only_member_directories_and_stop_at_the_sentence() {
        let dirs = ["wcl-core", "wcl", "crates/a"];
        let is = |d: &str| dirs.contains(&d);
        let read = |t: &str| prose_members(t, &is);
        assert_eq!(read("with members wcl-core and wcl."), ["wcl-core", "wcl"]);
        assert_eq!(
            read("two member crates: `wcl-core`, `wcl`"),
            ["wcl-core", "wcl"]
        );
        assert_eq!(read("Members: crates/a/ and wcl"), ["crates/a", "wcl"]);
        assert_eq!(read("MEMBERS wcl-core, wcl"), ["wcl-core", "wcl"]);
        // A word that is no member directory is prose, not a member.
        assert!(read("the members listed below").is_empty());
        // The list ends at its sentence, a semicolon or a parenthesis.
        assert_eq!(read("members wcl-core. Then write wcl."), ["wcl-core"]);
        assert_eq!(read("members wcl-core; c2 adds wcl"), ["wcl-core"]);
        assert_eq!(read("members wcl-core (wcl comes later)"), ["wcl-core"]);
        // A word that ends the list: what follows is not listed now.
        assert_eq!(read("members wcl-core, not wcl"), ["wcl-core"]);
        assert_eq!(read("members wcl-core until c2 adds wcl"), ["wcl-core"]);
        // Only whole words: `remember`, `membership` name no list.
        assert!(read("remember wcl; membership wcl").is_empty());
        // Names before the word are not read.
        assert!(read("list only wcl-core in the members").is_empty());
    }

    /// A list in prose is a fallback: a `members = [...]` list wins.
    #[test]
    fn a_toml_member_list_wins_over_prose() {
        let [mut c1, c2] = round10_plan();
        c1.instruction = "Write Cargo.toml with members = [\"wcl-core\"] (members wcl-core and \
                          wcl come later)."
            .into();
        assert!(refusal(&[c1, c2], &[], &Fake::default()).is_none());
    }

    #[test]
    fn a_workspace_acceptance_is_accepted_when_the_members_come_first_or_exist() {
        // The member's contract is a dependency of the one that builds the
        // whole workspace. The member's own check runs from its manifest:
        // with no root manifest yet, cargo builds it alone.
        let mut specs = greenfield(&["c2"], &[]);
        specs[1].acceptance = "cargo test --manifest-path wcl/Cargo.toml".into();
        assert!(
            refusal(&specs, &[], &Fake::default()).is_none(),
            "{:?}",
            refusal(&specs, &[], &Fake::default())
        );
        // The member already exists in the repository, and its contract
        // runs after the root's.
        let repo = Fake::with(&[("wcl/Cargo.toml", "[package]\nname = \"wcl\"\n")]);
        let mut specs = greenfield(&[], &["c1"]);
        specs[1].acceptance = "cargo test -p wcl".into();
        assert!(refusal(&specs, &[], &repo).is_none());
        // One contract writes the whole workspace.
        let one = [spec(
            "c1",
            "Create the workspace: `Cargo.toml` with members = [\"a\", \"b\"]",
            "cargo build --workspace",
            &[
                "Cargo.toml",
                "a/Cargo.toml",
                "a/src/lib.rs",
                "b/Cargo.toml",
                "b/src/main.rs",
            ],
            &[],
        )];
        assert!(refusal(&one, &[], &Fake::default()).is_none());
    }

    /// A plan that is already fine is not refused: an existing workspace
    /// whose members are all there, contracts that edit their own crates,
    /// and acceptances that are not cargo.
    #[test]
    fn a_plan_in_a_complete_workspace_is_not_refused() {
        let repo = Fake::with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"core\", \"cli\", \"crates/*\"]\nexclude = [\"crates/old\"]\n",
            ),
            ("core/Cargo.toml", "[package]\nname = \"core\"\n"),
            ("core/src/lib.rs", "pub fn f() {}\n"),
            ("cli/Cargo.toml", "[package]\nname = \"cli\"\n"),
            ("cli/src/main.rs", "fn main() {}\n"),
            ("crates/io/Cargo.toml", "[package]\nname = \"io\"\n"),
        ]);
        let specs = [
            spec(
                "a",
                "edit core",
                "cargo test -p core",
                &["core/src/lib.rs"],
                &[],
            ),
            spec(
                "b",
                "edit cli",
                "cd cli && cargo test",
                &["cli/src/main.rs"],
                &[],
            ),
            spec(
                "c",
                "add io",
                "cargo test --workspace",
                &["crates/io/src/lib.rs"],
                &[],
            ),
            spec("d", "docs", "test -f README.md", &["README.md"], &[]),
            spec(
                "e",
                "old crate, excluded",
                "cargo test -p core",
                &["crates/old/src/lib.rs"],
                &[],
            ),
        ];
        let got = refusal(&specs, &[], &repo);
        assert!(got.is_none(), "{got:?}");
    }

    /// This repository's own workspace, read from disk: ordinary plans in a
    /// complete workspace, in every cargo form, are not refused.
    #[test]
    fn this_repository_with_ordinary_plans_is_not_refused() {
        let root = kgraf_core::plain_path::canonicalize(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        )
        .expect("root");
        let repo = DiskRepo(root);
        assert!(
            repo_members("Cargo.toml", &repo).is_some_and(|m| !m.is_empty()),
            "the root manifest is read as a workspace"
        );
        let specs = [
            spec(
                "c1",
                "edit the plan check",
                "cargo test -p kgraf-orchestrator --lib plan_repo",
                &["crates/kgraf-orchestrator/src/plan_repo.rs"],
                &[],
            ),
            spec(
                "c2",
                "add a crate",
                "cargo clippy --workspace --all-targets -- -D warnings",
                &["crates/kgraf-new/Cargo.toml", "crates/kgraf-new/src/lib.rs"],
                &[],
            ),
            spec(
                "c3",
                "edit the host",
                "cd crates/kgraf-host && cargo test",
                &["crates/kgraf-host/src/lib.rs"],
                &[],
            ),
        ];
        let got = refusal(&specs, &[], &repo);
        assert!(got.is_none(), "{got:?}");
    }

    /// A directory a contract writes into under a `<prefix>/*` member is a
    /// member; cargo refuses it without a manifest (measured with an empty
    /// directory and with one that holds only sources).
    #[test]
    fn a_glob_member_directory_without_a_manifest_is_refused() {
        let repo = Fake::with(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ]);
        let specs = [
            spec(
                "c1",
                "add crate b",
                "cargo test -p a",
                &["crates/b/src/lib.rs"],
                &[],
            ),
            spec(
                "c2",
                "b's manifest",
                "cargo test -p b",
                &["crates/b/Cargo.toml"],
                &[],
            ),
        ];
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("`crates/b/Cargo.toml`"), "{why}");
        assert!(why.contains("`c1`"), "{why}");
    }

    #[test]
    fn a_workspace_manifest_only_a_later_contract_writes_is_refused() {
        // c2's acceptance runs at the root, whose manifest only c1 writes.
        let repo = Fake::default();
        let mut specs = greenfield(&[], &[]);
        specs[0].acceptance = "cargo test -p wcl-core".into();
        specs[0].writes.push(WriteTarget {
            path: "wcl/Cargo.toml".into(),
            node: None,
        });
        specs[0].writes.push(WriteTarget {
            path: "wcl/src/main.rs".into(),
            node: None,
        });
        specs[1].writes.retain(|w| w.path != "wcl/Cargo.toml");
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("`c2`"), "{why}");
        assert!(why.contains("workspace manifest `Cargo.toml`"), "{why}");
        assert!(why.contains("cargo finds no manifest"), "{why}");
        // The `-p` form at the root fails the same way (measured: "could
        // not find `Cargo.toml`").
        specs[1].acceptance = "cargo test -p wcl".into();
        let why = reason(refusal(&specs, &[], &repo));
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
        // A plan that does not write the root manifest is checked against
        // the repository's too.
        let specs = [spec(
            "c1",
            "edit a",
            "cargo test -p a",
            &["a/src/lib.rs"],
            &[],
        )];
        let why = reason(refusal(&specs, &[], &repo));
        assert!(why.contains("`b/Cargo.toml`"), "{why}");
    }

    #[test]
    fn the_command_and_member_readers_are_exact() {
        let runs = |a: &str| -> Vec<(String, String)> {
            cargo_runs(a)
                .into_iter()
                .map(|r| (r.command, r.dir))
                .collect()
        };
        let one = |c: &str, d: &str| vec![(c.to_owned(), d.to_owned())];
        assert_eq!(
            runs("run `cargo test --workspace` locally"),
            one("cargo test --workspace", "")
        );
        assert_eq!(
            runs("cargo clippy --all -- -D warnings"),
            one("cargo clippy --all", "")
        );
        assert_eq!(
            runs("cargo test -p wcl-core && echo SUCCESS"),
            one("cargo test -p wcl-core", "")
        );
        assert_eq!(
            runs("cargo test --manifest-path crates/a/Cargo.toml"),
            one("cargo test --manifest-path crates/a/Cargo.toml", "crates/a")
        );
        assert_eq!(
            runs("cd crates && cargo -C a build"),
            one("cargo build", "crates/a")
        );
        assert_eq!(
            runs("(cd a && cargo test); cargo build"),
            vec![
                ("cargo test".to_owned(), "a".to_owned()),
                ("cargo build".to_owned(), String::new())
            ]
        );
        assert_eq!(
            runs("cargo build and cargo test --offline"),
            vec![
                ("cargo build".to_owned(), String::new()),
                ("cargo test --offline".to_owned(), String::new())
            ]
        );
        for no in [
            "cargo new x",
            "cargo install ripgrep",
            "the cargo is loaded",
            "cd && cargo test",
            "cd $HOME && cargo test",
            "cargo test --manifest-path /abs/Cargo.toml",
        ] {
            assert!(runs(no).is_empty(), "{no}: {:?}", runs(no));
        }
        assert_eq!(
            members_in("with members `[\"wcl-core\", \"wcl\"]` and more"),
            vec!["wcl-core".to_owned(), "wcl".to_owned()]
        );
        assert_eq!(members_in("members = ['a']"), vec!["a".to_owned()]);
        assert!(members_in("members of the team [\"x\"]").is_empty());
        assert_eq!(
            Members::from_list(
                members_in("members = [\"crates/*\", \"x/**\", \"b\"]"),
                vec![]
            ),
            Members {
                explicit: vec!["b".into()],
                globs: vec!["crates".into()],
                exclude: vec![],
            }
        );
    }

    fn open(id: &str, acceptance: &str, writes: &[&str], deps: &[&str]) -> OpenContract {
        let mut c = OpenContract::planned(id, kgraf_core::TaskId::new(id), "", acceptance);
        c.writes = writes
            .iter()
            .map(|p| WriteTarget {
                path: (*p).into(),
                node: None,
            })
            .collect();
        c.depends_on = deps.iter().map(|d| (*d).to_owned()).collect();
        c
    }

    /// Round 8: attempt 2 of c1-core asked three times for `wcl/Cargo.toml`
    /// and was told "do not edit it; your attempt is judged by its own
    /// acceptance check", which could not pass. That report now holds the
    /// contract for a re-plan; a file the check does not need, or one a
    /// contract that runs first writes, does not.
    #[test]
    fn a_member_manifest_a_later_contract_writes_holds_for_a_replan() {
        let [s1, s2] = round8_second_plan();
        let mut c1 = open(
            "c1-core",
            "cargo test -p wcl-core && echo SUCCESS",
            &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"],
            &[],
        );
        c1.instruction = s1.instruction;
        let mut c2 = open(
            "c2-bin",
            "cargo test --workspace",
            &["wcl/Cargo.toml", "wcl/src/main.rs"],
            &["c1-core"],
        );
        c2.instruction = s2.instruction;
        let plan = [c1, c2];
        let repo = Fake::default();
        let hold = member_hold(&plan, "c1-core", &["./wcl/Cargo.toml"], &repo).expect("held");
        assert!(hold.contains("`wcl/Cargo.toml`"), "{hold}");
        assert!(hold.contains("plan-shape error"), "{hold}");
        assert!(hold.contains("Re-plan"), "{hold}");
        assert!(hold.contains("cargo test -p wcl-core"), "{hold}");
        assert!(!hold.contains("judged by its own acceptance"), "{hold}");
        // The source the remedy writes with it holds too.
        assert!(member_hold(&plan, "c1-core", &["wcl/src/main.rs"], &repo).is_some());
        // A file the check does not need: answered as before.
        assert!(member_hold(&plan, "c1-core", &["README.md"], &repo).is_none());
        // c2-bin depends on c1-core: what it needs comes first.
        assert!(member_hold(&plan, "c2-bin", &["Cargo.toml"], &repo).is_none());
        // A member no open contract writes is the plan-shape hold of
        // `decide_in_plan`, not this one.
        let mut alone = plan.clone();
        alone[1].writes.clear();
        assert!(member_hold(&alone, "c1-core", &["wcl/Cargo.toml"], &repo).is_none());
    }

    /// Writes the files contract `id` can count on, its own and its
    /// dependencies' writes over `repo`, with minimal contents as a worker
    /// that did its part leaves them, and runs its acceptance with cargo.
    #[cfg(unix)]
    fn cargo_passes(specs: &[ContractSpec], id: &str, repo: &[(&str, &str)]) -> (bool, String) {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        let put = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        };
        for (rel, text) in repo {
            put(rel, text);
        }
        let fake = Fake::with(repo);
        let mut supplied = ancestors(specs, id);
        supplied.insert(id);
        for c in specs.iter().filter(|c| supplied.contains(c.id.as_str())) {
            for w in &c.writes {
                let rel = norm(&w.path);
                let text = if rel == "Cargo.toml" {
                    let m = seen_members(specs, &supplied, rel, &fake).unwrap_or_default();
                    let list = m
                        .explicit
                        .iter()
                        .map(|m| format!("\"{m}\""))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("[workspace]\nmembers = [{list}]\nresolver = \"2\"\n")
                } else if let Some(d) = rel.strip_suffix("/Cargo.toml") {
                    let name = d.rsplit('/').next().unwrap_or(d);
                    format!(
                        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
                    )
                } else if rel.ends_with("src/lib.rs") {
                    "pub fn one() -> u8 { 1 }\n#[test]\nfn t() { assert_eq!(one(), 1); }\n"
                        .to_owned()
                } else if rel.ends_with("src/main.rs") {
                    "fn main() {}\n".to_owned()
                } else {
                    String::new()
                };
                put(rel, &text);
            }
        }
        let c = specs.iter().find(|c| c.id == id).expect("contract");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let bin = std::path::Path::new(&cargo)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let path = format!("{bin}:{}", std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&c.acceptance)
            .current_dir(root)
            .env("PATH", path)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("CARGO_NET_OFFLINE", "true")
            .output()
            .expect("sh");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// The check's verdict on each contract is cargo's: every contract the
    /// check refuses fails in a real cargo run over the files it can count
    /// on, and every contract it accepts passes. Covers the round 8 plan,
    /// both remedies, the manifest with no source, the root manifest that
    /// comes later, and a member built alone from its manifest.
    #[cfg(unix)]
    #[test]
    fn the_check_agrees_with_cargo() {
        let mut no_source = round8_second_plan();
        no_source[0].writes.push(WriteTarget {
            path: "wcl/Cargo.toml".into(),
            node: None,
        });
        // Commands only: the shell runs them as they are written.
        let mut root_later = greenfield(&[], &[]);
        root_later[0].acceptance = "cargo test --workspace".into();
        root_later[1].acceptance = "cargo test -p wcl".into();
        let mut alone = greenfield(&["c2"], &[]);
        alone[0].acceptance = "cargo test --workspace".into();
        alone[1].acceptance = "cargo test --manifest-path wcl/Cargo.toml".into();
        let cases: Vec<(&str, Vec<ContractSpec>, bool)> = vec![
            ("round 8 plan", round8_second_plan().to_vec(), true),
            ("stub members", remedy_stub_members().to_vec(), false),
            (
                "members add themselves",
                remedy_members_add_themselves().to_vec(),
                false,
            ),
            ("manifest with no source", no_source.to_vec(), true),
            ("root manifest later", root_later.to_vec(), true),
            ("member alone", alone.to_vec(), false),
        ];
        let repo = Fake::default();
        for (name, specs, some_refused) in cases {
            let mut refused_any = false;
            for c in &specs {
                let findings = workspace_findings(&specs, c, &repo);
                let (passed, stderr) = cargo_passes(&specs, &c.id, &[]);
                assert_eq!(
                    findings.is_empty(),
                    passed,
                    "{name}, `{}`: check {findings:?}, cargo {stderr}",
                    c.id
                );
                // A failure is cargo's, on the workspace, not the shell's.
                if !passed {
                    assert!(
                        stderr.contains("failed to load manifest for workspace member")
                            || stderr.contains("could not find `Cargo.toml`"),
                        "{name}, `{}`: {stderr}",
                        c.id
                    );
                }
                refused_any |= !findings.is_empty();
            }
            assert_eq!(refused_any, some_refused, "{name}");
        }
    }

    /// A `go.work` that uses a module whose `go.mod` a later contract
    /// writes: every go command in the workspace fails on it, so the plan is
    /// refused naming the file. Written by the contract itself or one it
    /// depends on, or already in the repository: no refusal.
    #[test]
    fn a_go_work_member_written_later_is_refused_and_one_in_time_is_not() {
        let repo = Fake::with(&[
            (
                "go.work",
                "go 1.22\n\nuse (\n\t./core\n\t./cli // the binary\n)\n",
            ),
            ("core/go.mod", "module example.com/core\n"),
        ]);
        let late = vec![
            spec("c1", "core", "go test ./...", &["core/a.go"], &[]),
            spec(
                "c2",
                "cli",
                "go test ./...",
                &["cli/go.mod", "cli/main.go"],
                &["c1"],
            ),
        ];
        let why = reason(refusal(&late, &[], &repo));
        assert!(
            why.contains("`c1`")
                && why.contains("`cli/go.mod`")
                && why.contains("go.work")
                && why.contains("only by contract `c2`"),
            "{why}"
        );
        let in_time = vec![
            spec(
                "c1",
                "cli",
                "go test ./...",
                &["cli/go.mod", "cli/main.go"],
                &[],
            ),
            spec(
                "c2",
                "core",
                "cd core && go vet ./...",
                &["core/a.go"],
                &["c1"],
            ),
        ];
        assert!(refusal(&in_time, &[], &repo).is_none());
        // GOWORK=off loads no workspace, and `go fmt`-like verbs load nothing.
        let off = vec![spec(
            "c1",
            "core",
            "GOWORK=off go test ./...",
            &["core/a.go"],
            &[],
        )];
        assert!(refusal(&off, &[], &repo).is_none());
        let fmt = vec![spec("c1", "core", "go fmt ./...", &["core/a.go"], &[])];
        assert!(refusal(&fmt, &[], &repo).is_none());
    }

    /// A Maven reactor that lists a module whose `pom.xml` comes later is
    /// refused; a module only in an inactive profile, or behind a comment, is
    /// not loaded and is not judged.
    #[test]
    fn a_maven_module_written_later_is_refused_and_a_profile_module_is_not() {
        let pom = "<project>\n<modules>\n  <module>core</module>\n  <module>web</module>\n  \
                   <!-- <module>old</module> -->\n</modules>\n<profiles><profile><modules>\
                   <module>bench</module></modules></profile></profiles>\n</project>\n";
        let repo = Fake::with(&[("pom.xml", pom), ("core/pom.xml", "<project/>")]);
        let late = vec![
            spec("c1", "core", "mvn -q test", &["core/src/A.java"], &[]),
            spec("c2", "web", "mvn test", &["web/pom.xml"], &["c1"]),
        ];
        let why = reason(refusal(&late, &[], &repo));
        assert!(
            why.contains("`web/pom.xml`") && why.contains("maven"),
            "{why}"
        );
        assert!(!why.contains("bench") && !why.contains("old/"), "{why}");
        let in_time = vec![
            spec("c1", "web", "mvn test", &["web/pom.xml"], &[]),
            spec(
                "c2",
                "core",
                "./mvnw -f pom.xml verify",
                &["core/src/A.java"],
                &["c1"],
            ),
        ];
        assert!(refusal(&in_time, &[], &repo).is_none());
        // A command in a module's own directory builds that module only.
        let inner = vec![spec(
            "c1",
            "core",
            "cd core && mvn test",
            &["core/src/A.java"],
            &[],
        )];
        assert!(refusal(&inner, &[], &repo).is_none());
    }

    /// The tools measured or known not to load a missing member are never
    /// refused for one: npm and pnpm workspaces, Gradle settings, a Go
    /// workspace or Maven pom the plan itself rewrites (its content is not
    /// known).
    #[test]
    fn workspaces_that_do_not_load_a_missing_member_are_never_refused() {
        let repo = Fake::with(&[
            ("package.json", r#"{"workspaces":["a","b"]}"#),
            ("pnpm-workspace.yaml", "packages:\n  - a\n  - b\n"),
            ("settings.gradle", "include 'a', 'b'\n"),
            ("go.work", "use ./b\n"),
            ("pom.xml", "<modules><module>b</module></modules>"),
        ]);
        for acceptance in ["npm test", "pnpm -r test", "gradle test"] {
            let plan = vec![
                spec("c1", "a", acceptance, &["a/x.js"], &[]),
                spec("c2", "b", acceptance, &["b/package.json"], &["c1"]),
            ];
            assert!(refusal(&plan, &[], &repo).is_none(), "{acceptance}");
        }
        let rewritten = vec![
            spec(
                "c1",
                "a",
                "go test ./... && mvn test",
                &["go.work", "pom.xml"],
                &[],
            ),
            spec(
                "c2",
                "b",
                "go test ./...",
                &["b/go.mod", "b/pom.xml"],
                &["c1"],
            ),
        ];
        assert!(refusal(&rewritten, &[], &repo).is_none());
    }
}
