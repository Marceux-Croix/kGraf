//! What the planner is told about the project beyond the instruction: the
//! package names in the manifests and the public signatures of the items the
//! instruction names.
//!
//! A planner that sees only an instruction and a file list invents the rest:
//! a crate name that is not the package's, a trait method with another
//! parameter count, a struct field that does not exist. Those plans fail their
//! interface checks at the gate, or worse, compile against nothing real. The
//! text here is the cheapest ground truth: manifests (a few lines each) and
//! signatures from the code graph, which stores one per node. Whole-file reads
//! are the fallback for a file the instruction names and the graph holds no
//! signature for, and each one is noticed.
//!
//! Bounded: the whole section is at most [`CAP_BYTES`]. Manifests get the first
//! [`MANIFEST_BYTES`]; signatures follow in the order the instruction names
//! them, and what does not fit is cut with a count of the cut lines.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use kgraf_graph::{GraphStore, NodeKind, NodeRecord};

/// The most the section adds to a planner prompt, in bytes.
pub const CAP_BYTES: usize = 8 * 1024;
/// The share of [`CAP_BYTES`] the manifests may take.
pub const MANIFEST_BYTES: usize = 3 * 1024;
/// Room kept for the closing cut note.
const CUT_NOTE_BYTES: usize = 200;
/// Longest signature kept on one line.
const SIGNATURE_BYTES: usize = 240;
/// Most of a trait, struct, enum or class declaration kept.
const DECLARATION_BYTES: usize = 1500;
/// Manifests read at most.
const MAX_MANIFESTS: usize = 40;
/// Identifier tokens of the instruction looked up in the graph at most.
const MAX_TOKENS: usize = 40;
/// Candidates kept for one name (two files may define one).
const MAX_CANDIDATES: usize = 3;

/// The section and the notices it raised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectContext {
    /// Empty when the project gave nothing to say.
    pub text: String,
    /// Said with the run's notices: a cut, a file read for want of signatures,
    /// a lookup that failed.
    pub notices: Vec<String>,
}

/// Build the section for `instruction` from the manifests under the graph's
/// root and the graph's own nodes. `known_files` are the graph's file paths.
pub fn project_context(
    graph: &GraphStore,
    instruction: &str,
    known_files: &[String],
) -> ProjectContext {
    let mut notices = Vec::new();
    let manifests = manifest_lines(graph.root(), known_files);
    let (manifest_text, manifest_cut) = fit(&manifests, MANIFEST_BYTES);
    if manifest_cut > 0 {
        notices.push(format!(
            "planner context: {manifest_cut} manifest lines were cut at {MANIFEST_BYTES} bytes"
        ));
    }

    let mut sigs = SignatureLines::default();
    for file in named_files(instruction, known_files) {
        sigs.add_file(graph, &file, &mut notices);
    }
    for name in named_symbols(instruction) {
        sigs.add_symbol(graph, &name, &mut notices);
    }
    let room = CAP_BYTES
        .saturating_sub(manifest_text.len())
        .saturating_sub(CUT_NOTE_BYTES);
    let (sig_text, sig_cut) = fit(&sigs.lines, room);
    if sig_cut > 0 {
        notices.push(format!(
            "planner context: {sig_cut} signature lines were cut at the {CAP_BYTES}-byte cap"
        ));
    }

    let mut text = String::new();
    if !manifest_text.is_empty() {
        text.push_str(
            "\n\nPackages in the repository (from the manifests; use these exact names in \
             imports and in -p arguments, a Rust package name with - is imported with _):\n",
        );
        text.push_str(&manifest_text);
    }
    if !sig_text.is_empty() {
        text.push_str(
            "\n\nSignatures of items the instruction names (from the code graph; a contract \
             must match them exactly):\n",
        );
        text.push_str(&sig_text);
    }
    if manifest_cut + sig_cut > 0 {
        let _ = writeln!(
            text,
            "[cut: {} lines did not fit the {CAP_BYTES}-byte cap]",
            manifest_cut + sig_cut
        );
    }
    ProjectContext { text, notices }
}

/// The lines that fit `limit` bytes, and how many were left out.
fn fit(lines: &[String], limit: usize) -> (String, usize) {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if out.len() + line.len() + 1 > limit {
            return (out, lines.len() - i);
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, 0)
}

// ------------------------------------------------------------- manifests

/// One line per package: its name, the manifest path, and for a Rust
/// workspace root its members.
fn manifest_lines(root: &Path, known_files: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let push = |p: String, paths: &mut Vec<String>| {
        if !paths.contains(&p) {
            paths.push(p);
        }
    };
    for name in ["Cargo.toml", "package.json", "pyproject.toml"] {
        if root.join(name).is_file() {
            push(name.to_owned(), &mut paths);
        }
    }
    for f in known_files {
        if is_manifest(f) {
            push(f.clone(), &mut paths);
        }
    }
    // A workspace's members, which the graph may not index.
    let mut members: Vec<String> = Vec::new();
    for p in &paths {
        if p.ends_with("Cargo.toml") {
            members.extend(workspace_members(root, p));
        }
    }
    for m in members {
        push(m, &mut paths);
    }
    paths.sort();
    paths.truncate(MAX_MANIFESTS);
    let mut lines = Vec::new();
    for p in &paths {
        let Ok(text) = std::fs::read_to_string(root.join(p)) else {
            continue;
        };
        if let Some(line) = manifest_line(p, &text) {
            lines.push(line);
        }
    }
    lines
}

fn is_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml")
}

/// The manifests of the members a workspace root names (`members = ["a",
/// "crates/*"]`): a trailing `/*` lists that folder one level deep.
fn workspace_members(root: &Path, manifest: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(manifest)) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(list) = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    else {
        return Vec::new();
    };
    let base = manifest.strip_suffix("Cargo.toml").unwrap_or("");
    let mut out = Vec::new();
    for m in list.iter().filter_map(|m| m.as_str()) {
        let m = m.trim_start_matches("./");
        if let Some(dir) = m.strip_suffix("/*") {
            let Ok(entries) = std::fs::read_dir(root.join(base).join(dir)) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().join("Cargo.toml").is_file())
                .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                .collect();
            names.sort();
            out.extend(
                names
                    .into_iter()
                    .map(|n| format!("{base}{dir}/{n}/Cargo.toml")),
            );
        } else if !m.contains('*') {
            out.push(format!("{base}{m}/Cargo.toml"));
        }
    }
    out
}

fn manifest_line(path: &str, text: &str) -> Option<String> {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file {
        "Cargo.toml" => {
            let table = text.parse::<toml::Table>().ok()?;
            let name = table
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str());
            let members = table
                .get("workspace")
                .and_then(|w| w.get("members"))
                .and_then(|m| m.as_array())
                .map(|m| {
                    m.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
            match (name, members) {
                (Some(n), Some(m)) => Some(format!(
                    "- {path}: package `{n}` (import name `{}`); workspace members: {m}",
                    n.replace('-', "_")
                )),
                (Some(n), None) => Some(format!(
                    "- {path}: package `{n}` (import name `{}`)",
                    n.replace('-', "_")
                )),
                (None, Some(m)) => Some(format!("- {path}: workspace members: {m}")),
                (None, None) => None,
            }
        }
        "package.json" => {
            let v: serde_json::Value = serde_json::from_str(text).ok()?;
            let n = v.get("name")?.as_str()?;
            Some(format!("- {path}: package `{n}`"))
        }
        "pyproject.toml" => {
            let table = text.parse::<toml::Table>().ok()?;
            let name = table
                .get("project")
                .and_then(|p| p.get("name"))
                .or_else(|| {
                    table
                        .get("tool")
                        .and_then(|t| t.get("poetry"))
                        .and_then(|p| p.get("name"))
                })
                .and_then(|n| n.as_str())?;
            Some(format!("- {path}: project `{name}`"))
        }
        _ => None,
    }
}

// ------------------------------------------------------------ signatures

/// The graph files whose path the instruction writes out, or whose file name
/// it writes out when no other file has that name.
fn named_files(instruction: &str, known_files: &[String]) -> Vec<String> {
    let mut out: Vec<String> = known_files
        .iter()
        .filter(|f| instruction.contains(f.as_str()))
        .cloned()
        .collect();
    for f in known_files {
        let base = f.rsplit('/').next().unwrap_or(f);
        let unique = known_files
            .iter()
            .filter(|g| g.rsplit('/').next().unwrap_or(g) == base)
            .count()
            == 1;
        if unique && base.len() >= 5 && instruction.contains(base) && !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

/// Names a model writes to point at code: backtick spans, and bare words that
/// look like identifiers (an underscore, an inner capital, or a call
/// paren). Plain prose words are not looked up.
fn named_symbols(instruction: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut take = |tok: &str, out: &mut Vec<String>| {
        let tok = tok.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
        if tok.len() >= 3 && seen.insert(tok.to_owned()) {
            out.push(tok.to_owned());
        }
    };
    for (i, span) in instruction.split('`').enumerate() {
        if i % 2 == 1 {
            // A span may be a path or a call: keep its identifier-shaped parts.
            for part in span.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')) {
                take(part, &mut out);
            }
        }
    }
    let bytes = instruction.as_bytes();
    let mut start = None;
    for (i, c) in instruction.char_indices().chain([(instruction.len(), ' ')]) {
        let word = c.is_alphanumeric() || c == '_';
        match (word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let tok = &instruction[s..i];
                let call = bytes.get(i) == Some(&b'(');
                let inner_capital = tok.chars().skip(1).any(char::is_uppercase)
                    && tok.chars().next().is_some_and(char::is_alphabetic);
                if tok.contains('_') || inner_capital || call {
                    take(tok, &mut out);
                }
                start = None;
            }
            _ => {}
        }
    }
    out.truncate(MAX_TOKENS);
    out
}

#[derive(Default)]
struct SignatureLines {
    lines: Vec<String>,
    taken: BTreeSet<String>,
}

impl SignatureLines {
    fn line(&mut self, graph: &GraphStore, n: &NodeRecord) {
        if n.signature.trim().is_empty() || !self.taken.insert(n.id.clone()) {
            return;
        }
        // A trait, struct, enum or class is its declaration: the header alone
        // names no method and no field, which is what a plan gets wrong. Its
        // text is read through the graph, bounded per item.
        if matches!(
            n.kind,
            NodeKind::Trait
                | NodeKind::Struct
                | NodeKind::Enum
                | NodeKind::Interface
                | NodeKind::Class
        ) && let Ok(text) = graph.source_of(n)
        {
            let mut cut = text.len().min(DECLARATION_BYTES);
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let more = if cut < text.len() { "\n...\n" } else { "" };
            self.lines.push(format!(
                "{}:{}\n{}{more}",
                n.path,
                n.start_line,
                text[..cut].trim_end()
            ));
            return;
        }
        let mut sig = n.signature.split_whitespace().collect::<Vec<_>>().join(" ");
        if sig.len() > SIGNATURE_BYTES {
            let mut end = SIGNATURE_BYTES;
            while !sig.is_char_boundary(end) {
                end -= 1;
            }
            sig.truncate(end);
            sig.push_str(" ...");
        }
        self.lines
            .push(format!("{}:{}  {}", n.path, n.start_line, sig));
    }

    /// Every signature in a file; its text up to the cap when the graph holds
    /// none for it.
    fn add_file(&mut self, graph: &GraphStore, path: &str, notices: &mut Vec<String>) {
        let nodes = match graph.nodes_in_file(path) {
            Ok(n) => n,
            Err(e) => {
                notices.push(format!(
                    "planner context: nodes of `{path}` unreadable: {e}"
                ));
                return;
            }
        };
        let before = self.lines.len();
        for n in nodes.iter().filter(|n| n.kind != NodeKind::File) {
            self.line(graph, n);
        }
        if self.lines.len() > before {
            return;
        }
        let Some(file) = nodes.iter().find(|n| n.kind == NodeKind::File) else {
            return;
        };
        if !self.taken.insert(file.id.clone()) {
            return;
        }
        match graph.source_of(file) {
            Ok(text) => {
                let mut cut = text.len().min(CAP_BYTES - MANIFEST_BYTES - CUT_NOTE_BYTES);
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                notices.push(format!(
                    "planner context: the graph holds no signatures for `{path}`, so its text \
                     is included up to {cut} of {} bytes",
                    text.len()
                ));
                self.lines
                    .push(format!("{path} (text, no signatures indexed):"));
                self.lines.extend(text[..cut].lines().map(str::to_owned));
            }
            Err(e) => notices.push(format!(
                "planner context: `{path}` has no signatures and its text is unreadable: {e}"
            )),
        }
    }

    /// The signature of each node a name resolves to, and of what it holds
    /// (a trait's methods, a struct's fields and impls).
    fn add_symbol(&mut self, graph: &GraphStore, name: &str, notices: &mut Vec<String>) {
        let found = match graph.resolve_symbol(name) {
            Ok(f) => f,
            Err(_) => return,
        };
        for node in found
            .iter()
            .filter(|n| n.kind != NodeKind::File)
            .take(MAX_CANDIDATES)
        {
            self.line(graph, node);
            let in_file = match graph.nodes_in_file(&node.path) {
                Ok(n) => n,
                Err(e) => {
                    notices.push(format!(
                        "planner context: nodes of `{}` unreadable: {e}",
                        node.path
                    ));
                    continue;
                }
            };
            // Emission order puts a parent before its children.
            let mut inside: BTreeSet<&str> = BTreeSet::from([node.id.as_str()]);
            for n in &in_file {
                if n.parent_id.as_deref().is_some_and(|p| inside.contains(p)) {
                    inside.insert(n.id.as_str());
                    self.line(graph, n);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_graph::Indexer;

    /// A workspace of two crates, a trait with a three-parameter method, and a
    /// struct.
    fn fixture() -> (tempfile::TempDir, GraphStore, Vec<String>) {
        let dir = tempfile::tempdir().expect("dir");
        let w = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, body).expect("write");
        };
        w(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
        );
        w(
            "crates/counter/Cargo.toml",
            "[package]\nname = \"fx-impl-counter\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        w(
            "crates/counter/src/lib.rs",
            "pub struct Receipt {\n    pub total_cents: u64,\n}\n\n\
             pub trait Pricing {\n    fn discount_cents(&self, subtotal: u64, code: &str) -> u64;\n}\n",
        );
        w(
            "crates/other/Cargo.toml",
            "[package]\nname = \"fx_other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        w("crates/other/src/lib.rs", "pub fn other() {}\n");
        let g = GraphStore::open_in_memory(dir.path()).expect("graph");
        Indexer::new(&g).index().expect("index");
        let files = g.file_paths().expect("files");
        (dir, g, files)
    }

    /// The prompt for a fixture workspace carries the package name from the
    /// manifest and the trait method's real signature.
    #[test]
    fn the_context_has_the_crate_name_and_the_trait_signature() {
        let (_dir, g, files) = fixture();
        let ctx = project_context(
            &g,
            "Implement `Pricing` for a counter and return a `Receipt`.",
            &files,
        );
        assert!(
            ctx.text
                .contains("package `fx-impl-counter` (import name `fx_impl_counter`)"),
            "{}",
            ctx.text
        );
        assert!(ctx.text.contains("package `fx_other`"), "{}", ctx.text);
        assert!(
            ctx.text
                .contains("fn discount_cents(&self, subtotal: u64, code: &str) -> u64"),
            "{}",
            ctx.text
        );
        assert!(ctx.text.contains("pub struct Receipt"), "{}", ctx.text);
        assert!(ctx.text.len() <= CAP_BYTES, "{}", ctx.text.len());
    }

    /// A file the instruction names is listed by signature, not by text.
    #[test]
    fn a_named_file_is_listed_by_its_signatures() {
        let (_dir, g, files) = fixture();
        let ctx = project_context(&g, "Edit crates/counter/src/lib.rs to add a field.", &files);
        assert!(ctx.text.contains("pub trait Pricing"), "{}", ctx.text);
        assert!(ctx.notices.is_empty(), "{:?}", ctx.notices);
    }

    /// A file with no indexed signatures is included as text, up to the cap,
    /// with a notice.
    #[test]
    fn a_named_file_without_signatures_is_included_as_text_with_a_notice() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("notes.rs"), "// line one\n// line two\n").expect("write");
        let g = GraphStore::open_in_memory(dir.path()).expect("graph");
        Indexer::new(&g).index().expect("index");
        let files = g.file_paths().expect("files");
        let ctx = project_context(&g, "Rewrite notes.rs.", &files);
        assert!(ctx.text.contains("line two"), "{}", ctx.text);
        assert!(
            ctx.notices.iter().any(|n| n.contains("no signatures")),
            "{:?}",
            ctx.notices
        );
    }

    /// Past the cap, signatures are cut with a count and a notice, and the
    /// section never exceeds the cap.
    #[test]
    fn the_section_is_capped_and_says_what_was_cut() {
        let dir = tempfile::tempdir().expect("dir");
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!(
                "pub fn item_number_{i}(a: u64, b: u64) -> u64 {{ a + b }}\n"
            ));
        }
        std::fs::write(dir.path().join("big.rs"), src).expect("write");
        let g = GraphStore::open_in_memory(dir.path()).expect("graph");
        Indexer::new(&g).index().expect("index");
        let files = g.file_paths().expect("files");
        let ctx = project_context(&g, "Change big.rs", &files);
        assert!(ctx.text.len() <= CAP_BYTES, "{}", ctx.text.len());
        assert!(ctx.text.contains("[cut: "), "{}", ctx.text);
        assert!(
            ctx.notices
                .iter()
                .any(|n| n.contains("signature lines were cut")),
            "{:?}",
            ctx.notices
        );
    }

    #[test]
    fn prose_words_are_not_looked_up_but_identifiers_are() {
        let names =
            named_symbols("Make the counter use `Pricing` and call discount_cents(x), FooBar too.");
        assert!(names.contains(&"Pricing".to_owned()), "{names:?}");
        assert!(names.contains(&"discount_cents".to_owned()), "{names:?}");
        assert!(names.contains(&"FooBar".to_owned()), "{names:?}");
        assert!(!names.contains(&"counter".to_owned()), "{names:?}");
    }
}
