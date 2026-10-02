//! A compile check derived from the repository when the planner wrote no
//! interface kGraf could accept (decision 0030, amendment of 2026-10-02).
//!
//! The planner is asked for interfaces on a bounded budget. When it still has
//! not bound a consumed contract, kGraf does not hold the run for an artifact
//! the planner could not write, and does not skip the check without a word.
//! Where the repository already holds the producer's scaffold (a file with a
//! placeholder body such as `todo!()` or `raise NotImplementedError`), that
//! scaffold is the stub, the repository files that use the producer's items
//! are the consumer and test files, and the project's own build tool compiles
//! them: stub-compile and its canaries run. Nothing correct exists to pass a
//! test, so the vacuity check cannot run; the edges stay counted as not run,
//! with the compile result in the reason.
//!
//! The decisions here are pure (what is a scaffold, a test file, which
//! command builds the files); the orchestrator gathers the facts from the
//! graph and the dispatch base.

use crate::checks::{FileOverlay, InterfaceContract};
use std::collections::BTreeSet;

/// Text that marks a body as not written yet, per language.
const SCAFFOLD_MARKERS: &[&str] = &[
    "todo!(",
    "unimplemented!(",
    "NotImplementedError",
    "not implemented",
    "Not implemented",
];

/// Whether `content` is a scaffold: it holds a placeholder body.
pub fn is_scaffold(content: &str) -> bool {
    SCAFFOLD_MARKERS.iter().any(|m| content.contains(m))
}

/// Whether `path` names a test file by the common conventions: under a
/// `tests`, `test`, `__tests__` or `spec` directory, or named `test_*`,
/// `*_test.*`, `*.test.*` or `*.spec.*`.
pub fn is_test_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let Some((name, dirs)) = parts.split_last() else {
        return false;
    };
    dirs.iter()
        .any(|d| matches!(*d, "tests" | "test" | "__tests__" | "spec"))
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// Whether `text` uses `name` as a whole word.
pub fn uses_word(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut from = 0;
    while let Some(at) = text[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before = start == 0 || !word(bytes[start - 1]);
        let after = end >= bytes.len() || !word(bytes[end]);
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// Names `content` declares, read by keyword: the fallback when the graph
/// holds no node for a file (not indexed yet, or a language it does not
/// parse). A token scan, not a parser: `def`, `class`, `fn`, `struct`,
/// `enum`, `trait`, `type`, `function`, `interface`, then an identifier.
pub fn declared_names(content: &str) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "def",
        "class",
        "fn",
        "struct",
        "enum",
        "trait",
        "type",
        "function",
        "interface",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut words = content
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .peekable();
    while let Some(w) = words.next() {
        if KEYWORDS.contains(&w)
            && let Some(name) = words.peek()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && !KEYWORDS.contains(name)
            && !out.iter().any(|o| o == name)
        {
            out.push((*name).to_owned());
        }
    }
    out
}

/// The package name a `Cargo.toml` declares under `[package]`.
pub fn cargo_package(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let v = value.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// The directories from `path`'s own up to the repository root (`""`).
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut dir = path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    loop {
        out.push(dir.clone());
        if dir.is_empty() {
            break;
        }
        dir = dir.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    }
    out
}

/// The Python module a repository-relative `.py` path imports as, when every
/// part is an identifier.
pub fn python_module(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".py")?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    let parts: Vec<&str> = stem.split('/').collect();
    let ident = |p: &str| {
        !p.is_empty()
            && !p.starts_with(|c: char| c.is_ascii_digit())
            && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    parts.iter().all(|p| ident(p)).then(|| parts.join("."))
}

/// The command that builds `files` with the project's own tool: for Rust
/// files, `cargo test --no-run` over the packages that hold them (`package`
/// gives a file's package); for Python files, an import of each module, which
/// resolves every name the module imports from the producer. `None` for any
/// other language, or a file whose package or module cannot be told.
pub fn compile_command(
    files: &[String],
    package: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    if files.is_empty() {
        return None;
    }
    if files.iter().all(|f| f.ends_with(".rs")) {
        let mut packages = BTreeSet::new();
        for f in files {
            packages.insert(package(f)?);
        }
        let mut argv = vec!["cargo".to_owned(), "test".to_owned(), "--no-run".to_owned()];
        for p in packages {
            argv.push("-p".to_owned());
            argv.push(p);
        }
        return Some(argv);
    }
    if files.iter().all(|f| f.ends_with(".py")) {
        let mut imports = Vec::new();
        for f in files {
            imports.push(format!("import {}", python_module(f)?));
        }
        return Some(vec![
            "python3".to_owned(),
            "-c".to_owned(),
            imports.join("; "),
        ]);
    }
    None
}

/// What the orchestrator found for one producer.
#[derive(Clone, Debug, Default)]
pub struct ProducerFacts {
    pub producer: String,
    /// The consuming contracts left unbound against this producer.
    pub consumers: Vec<String>,
    /// The producer's declared writes and their content at the dispatch base
    /// (`None`: no such file there).
    pub writes: Vec<(String, Option<String>)>,
    /// Names the producer's files define (graph nodes).
    pub symbols: Vec<String>,
    /// Repository files that use the producer's files (graph edges, the
    /// consumers' own existing files), with their content.
    pub users: Vec<(String, String)>,
}

/// A compile-only interface for `facts`, or why none can be derived. The
/// command is given by `command` over the consumer and test paths.
pub fn derive(
    facts: &ProducerFacts,
    command: impl Fn(&[String]) -> Option<Vec<String>>,
) -> Result<InterfaceContract, String> {
    let stub: Vec<FileOverlay> = facts
        .writes
        .iter()
        .filter_map(|(p, c)| {
            c.as_ref()
                .filter(|c| is_scaffold(c))
                .map(|c| FileOverlay::new(p, c))
        })
        .collect();
    if stub.is_empty() {
        return Err(format!(
            "no file {} writes holds a scaffold (a placeholder body) to stand in as its stub",
            facts.producer
        ));
    }
    let stub_paths: BTreeSet<&str> = stub.iter().map(|f| f.path.as_str()).collect();
    let mut symbols: Vec<String> = Vec::new();
    let mut consumer = Vec::new();
    let mut tests = Vec::new();
    for (path, content) in &facts.users {
        if stub_paths.contains(path.as_str()) {
            continue;
        }
        let used: Vec<&String> = facts
            .symbols
            .iter()
            .filter(|s| uses_word(content, s))
            .collect();
        if used.is_empty() {
            continue;
        }
        for s in used {
            if !symbols.contains(s) {
                symbols.push(s.clone());
            }
        }
        let file = FileOverlay::new(path, content);
        if is_test_path(path) {
            tests.push(file);
        } else {
            consumer.push(file);
        }
    }
    if consumer.is_empty() && tests.is_empty() {
        return Err(format!(
            "no repository file uses an item {}'s files define",
            facts.producer
        ));
    }
    let built: Vec<String> = consumer
        .iter()
        .chain(&tests)
        .map(|f| f.path.clone())
        .collect();
    let Some(compile) = command(&built) else {
        return Err(format!(
            "kGraf knows no build command for {} in this project",
            built.join(", ")
        ));
    };
    let mut parties = vec![facts.producer.clone()];
    parties.extend(facts.consumers.iter().cloned());
    Ok(InterfaceContract {
        id: format!("derived-{}", facts.producer),
        version: 1,
        parties,
        symbols,
        stub,
        wrong: Vec::new(),
        reference: Vec::new(),
        consumer,
        tests,
        compile,
        test: Vec::new(),
        consulted: Vec::new(),
        replaces: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ProducerFacts {
        ProducerFacts {
            producer: "c1".into(),
            consumers: vec!["c5".into()],
            writes: vec![(
                "discount_flat/src/lib.rs".into(),
                Some("pub struct FlatDiscount;\nfn d() -> i64 { todo!() }\n".into()),
            )],
            symbols: vec!["FlatDiscount".into(), "d".into()],
            users: vec![
                (
                    "pricing_engine/src/lib.rs".into(),
                    "use discount_flat::FlatDiscount;\npub fn price() { todo!() }\n".into(),
                ),
                ("checkout/tests/integration.rs".into(), "fn t() {}\n".into()),
            ],
        }
    }

    #[test]
    fn a_scaffold_becomes_the_stub_and_its_users_the_consumers() {
        let ic = derive(&facts(), |files| {
            compile_command(files, |f| {
                Some(f.split('/').next().unwrap_or("").to_owned())
            })
        })
        .expect("derived");
        assert_eq!(ic.stub[0].path, "discount_flat/src/lib.rs");
        assert_eq!(ic.consumer.len(), 1, "the integration test uses no symbol");
        assert!(ic.tests.is_empty());
        assert_eq!(ic.symbols, vec!["FlatDiscount".to_owned()]);
        assert_eq!(
            ic.compile,
            ["cargo", "test", "--no-run", "-p", "pricing_engine"]
        );
        assert_eq!(ic.parties, ["c1", "c5"]);
        assert!(ic.reference.is_empty(), "no reference: vacuity cannot run");
    }

    #[test]
    fn no_scaffold_or_no_user_or_no_command_derives_nothing_and_says_why() {
        let mut f = facts();
        f.writes[0].1 = Some("pub struct FlatDiscount;\n".into());
        assert!(
            derive(&f, |_| None)
                .expect_err("no scaffold")
                .contains("scaffold")
        );
        let mut f = facts();
        f.users.clear();
        assert!(derive(&f, |_| None).expect_err("no user").contains("uses"));
        assert!(
            derive(&facts(), |_| None)
                .expect_err("no command")
                .contains("build command")
        );
    }

    #[test]
    fn declared_names_are_read_by_keyword() {
        assert_eq!(
            declared_names(
                "class Config:\n    pass\ndef parse_config(text):\n    raise NotImplementedError\n"
            ),
            ["Config", "parse_config"]
        );
        assert_eq!(
            declared_names("pub struct FlatDiscount;\nfn d() {}"),
            ["FlatDiscount", "d"]
        );
    }

    #[test]
    fn test_paths_python_modules_and_cargo_packages_are_read_by_convention() {
        assert!(is_test_path("checkout/tests/integration.rs"));
        assert!(is_test_path("tests/test_library.py"));
        assert!(is_test_path("src/a.test.ts"));
        assert!(!is_test_path("src/testing.rs"));
        assert_eq!(
            python_module("shapes/library.py").as_deref(),
            Some("shapes.library")
        );
        assert_eq!(python_module("pkg/__init__.py").as_deref(), Some("pkg"));
        assert_eq!(python_module("my-pkg/x.py"), None);
        assert_eq!(
            cargo_package("[package]\nname = \"tax_us\"\n[dependencies]\nname = 1\n").as_deref(),
            Some("tax_us")
        );
        assert_eq!(ancestors("a/b/c.rs"), ["a/b", "a", ""]);
        assert_eq!(
            compile_command(&["tests/test_library.py".into()], |_| None),
            Some(vec![
                "python3".into(),
                "-c".into(),
                "import tests.test_library".into()
            ])
        );
    }
}
