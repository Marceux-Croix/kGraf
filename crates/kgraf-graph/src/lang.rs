//! Languages and their node-type specification tables.
//!
//! Chunking rules are *named per language in a table* rather than inferred, so
//! that what counts as a node is reviewable and so kGraf and
//! codebase-memory-mcp agree on the answer. Adding a language is adding a row
//! here, and a language with no row is indexed as an opaque file node rather
//! than silently chunked by some generic fallback.
//!
//! **Two tiers** ([`Tier`]). The five languages with a table here are the full
//! tier. The **tags tier** (Go, Java, C, C++, Ruby, C#, PHP) has no table of its
//! own: its definitions and call references come from the `TAGS_QUERY` that each
//! upstream grammar crate ships (`queries/tags.scm`, the query GitHub's code
//! navigation uses), so the rules are still named in one reviewable place, just
//! upstream's. Its edges are name-resolved only, like the full tier's, and its
//! nodes carry no signature-level disambiguation beyond what the query captures;
//! [`Language::tier`] lets a slice or the galaxy say so.

use crate::chunk::NodeKind;
use crate::error::{GraphError, Result};

/// The version of the chunker's own logic (`chunk.rs`, `index.rs`'s record
/// building). **Bump it when a change there makes the same bytes yield different
/// nodes, edges or call sites.** It is part of every language's
/// [`Language::extractor_stamp`]; the tables and grammars are hashed from the code,
/// so they need no bump.
pub const EXTRACTOR_VERSION: u32 = 1;

/// A language kGraf can chunk.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Markdown,
    // The tags tier.
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    CSharp,
    Php,
}

/// How a language's rows are extracted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// kGraf's own specification table ([`Language::spec`]).
    Full,
    /// The grammar crate's `TAGS_QUERY`: definitions and call references as
    /// upstream tags them. Call edges are name-resolved only.
    Tags,
}

/// One row of a language's specification table: a tree-sitter node type, the
/// kGraf node kind it becomes, and where its name is found.
#[derive(Clone, Copy, Debug)]
pub struct SpecRow {
    /// The tree-sitter node type, exactly as the grammar names it.
    pub ts_kind: &'static str,
    /// The kGraf kind it maps to.
    pub kind: NodeKind,
    /// How to recover the element's name from the matched node.
    pub name: NameRule,
}

/// How a matched node's name is recovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRule {
    /// Take the node's `name` field.
    NameField,
    /// Take the node's `type` field. Rust `impl` blocks name a type, not a name.
    TypeField,
    /// Take the first `identifier`-ish descendant. Used where a grammar does not
    /// expose a field, such as a decorated definition.
    FirstIdentifier,
    /// The node is a Markdown heading; its name is its own text with the
    /// leading `#` marks and surrounding whitespace removed.
    HeadingText,
}

impl Language {
    /// Recognize a language from a file path's extension.
    ///
    /// Returns `None` for paths kGraf does not chunk, which are skipped
    /// rather than indexed by some generic fallback.
    pub fn from_path(path: &str) -> Option<Language> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?;
        Some(match ext {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "md" | "markdown" => Language::Markdown,
            "go" => Language::Go,
            "java" => Language::Java,
            // A `.h` header is read as C; C++ headers conventionally use the
            // extensions below. A C++ header named `.h` still parses as C for
            // its C-shaped declarations.
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" => Language::Cpp,
            "rb" => Language::Ruby,
            "cs" => Language::CSharp,
            "php" => Language::Php,
            _ => return None,
        })
    }

    /// The language's stable name, for persistence and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Markdown => "markdown",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Ruby => "ruby",
            Language::CSharp => "csharp",
            Language::Php => "php",
        }
    }

    /// Parse a persisted language name.
    pub fn parse(name: &str) -> Option<Language> {
        Language::all().iter().copied().find(|l| l.as_str() == name)
    }

    /// Which tier extracts this language.
    pub fn tier(self) -> Tier {
        match self.tags_query() {
            Some(_) => Tier::Tags,
            None => Tier::Full,
        }
    }

    /// The grammar crate's own tags query, for a tags-tier language.
    pub fn tags_query(self) -> Option<&'static str> {
        Some(match self {
            Language::Go => tree_sitter_go::TAGS_QUERY,
            Language::Java => tree_sitter_java::TAGS_QUERY,
            Language::C => tree_sitter_c::TAGS_QUERY,
            Language::Cpp => tree_sitter_cpp::TAGS_QUERY,
            Language::Ruby => tree_sitter_ruby::TAGS_QUERY,
            Language::CSharp => tree_sitter_c_sharp::TAGS_QUERY,
            Language::Php => tree_sitter_php::TAGS_QUERY,
            _ => return None,
        })
    }

    /// The tree-sitter grammar for this language.
    pub fn grammar(self) -> Result<tree_sitter::Language> {
        let language: tree_sitter::Language = match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::Markdown => tree_sitter_md::LANGUAGE.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Java => tree_sitter_java::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            // PHP files open in HTML mode (`<?php ... ?>`); the PHP-only grammar
            // would fail on the opening tag.
            Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        };
        // Installing the grammar is what actually surfaces an ABI mismatch, so
        // do it here and let the error carry the language name.
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .map_err(|source| GraphError::Grammar {
                language: self.as_str(),
                source,
            })?;
        Ok(language)
    }

    /// The node types that become graph nodes in this language.
    pub fn spec(self) -> &'static [SpecRow] {
        macro_rules! rows {
            ($(($k:literal, $nk:expr, $nr:expr)),* $(,)?) => {
                &[$(SpecRow { ts_kind: $k, kind: $nk, name: $nr }),*]
            };
        }
        match self {
            Language::Rust => rows![
                ("function_item", NodeKind::Function, NameRule::NameField),
                ("struct_item", NodeKind::Struct, NameRule::NameField),
                ("enum_item", NodeKind::Enum, NameRule::NameField),
                ("union_item", NodeKind::Struct, NameRule::NameField),
                ("trait_item", NodeKind::Trait, NameRule::NameField),
                ("impl_item", NodeKind::Impl, NameRule::TypeField),
                ("mod_item", NodeKind::Module, NameRule::NameField),
                ("macro_definition", NodeKind::Macro, NameRule::NameField),
                ("type_item", NodeKind::TypeAlias, NameRule::NameField),
                ("const_item", NodeKind::Constant, NameRule::NameField),
                ("static_item", NodeKind::Constant, NameRule::NameField),
            ],
            Language::Python => rows![
                // `decorated_definition` wraps the thing it decorates. It is
                // listed first so the decorators land inside the node's span
                // rather than orphaned between siblings.
                (
                    "decorated_definition",
                    NodeKind::Decorated,
                    NameRule::FirstIdentifier
                ),
                (
                    "function_definition",
                    NodeKind::Function,
                    NameRule::NameField
                ),
                ("class_definition", NodeKind::Class, NameRule::NameField),
            ],
            Language::TypeScript | Language::Tsx | Language::JavaScript => rows![
                (
                    "function_declaration",
                    NodeKind::Function,
                    NameRule::NameField
                ),
                (
                    "generator_function_declaration",
                    NodeKind::Function,
                    NameRule::NameField
                ),
                ("class_declaration", NodeKind::Class, NameRule::NameField),
                (
                    "abstract_class_declaration",
                    NodeKind::Class,
                    NameRule::NameField
                ),
                ("method_definition", NodeKind::Method, NameRule::NameField),
                (
                    "interface_declaration",
                    NodeKind::Interface,
                    NameRule::NameField
                ),
                (
                    "type_alias_declaration",
                    NodeKind::TypeAlias,
                    NameRule::NameField
                ),
                ("enum_declaration", NodeKind::Enum, NameRule::NameField),
            ],
            Language::Markdown => rows![
                ("atx_heading", NodeKind::Section, NameRule::HeadingText),
                ("setext_heading", NodeKind::Section, NameRule::HeadingText),
            ],
            // The tags tier has no table of its own: `tags_query` names its rules.
            Language::Go
            | Language::Java
            | Language::C
            | Language::Cpp
            | Language::Ruby
            | Language::CSharp
            | Language::Php => &[],
        }
    }

    /// The tree-sitter node types that denote a call site in this language.
    ///
    /// Used to derive `calls` edges. The call graph is no longer the collision
    /// predictor (decision 0020) but it keeps its context-slice and navigation
    /// jobs, which is what these edges serve.
    pub fn call_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["call_expression", "macro_invocation"],
            Language::Python => &["call"],
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                &["call_expression", "new_expression"]
            }
            Language::Markdown => &[],
            // A tags-tier language's calls come from its query's `reference.call`
            // captures. The upstream C and C++ queries tag no calls, C#'s tags only
            // member calls (`reference.send`), and PHP's misses a call to a bare
            // name (`helper()`), so these name the call node too; the callee is its
            // `function` field, qualifier stripped (`chunk::callee_name`).
            Language::C | Language::Cpp => &["call_expression"],
            Language::CSharp => &["invocation_expression"],
            Language::Php => &["function_call_expression"],
            Language::Go | Language::Java | Language::Ruby => &[],
        }
    }

    /// The language family a call may resolve within. A call never resolves to a
    /// definition of another family: a Python `make()` is not a caller of a Rust
    /// `fn make`. TypeScript, TSX
    /// and JavaScript import each other, so they are one family; so do C and C++.
    pub fn family(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript | Language::Tsx | Language::JavaScript => "ecmascript",
            Language::Markdown => "markdown",
            Language::Go => "go",
            Language::Java => "java",
            Language::C | Language::Cpp => "c",
            Language::Ruby => "ruby",
            Language::CSharp => "csharp",
            Language::Php => "php",
        }
    }

    /// The identity of what extracts this language's rows: [`EXTRACTOR_VERSION`],
    /// the specification table, the call kinds, and the grammar itself (its name,
    /// ABI and metadata version, and every node kind and field name it defines).
    /// A different stamp means the same bytes may now yield different rows, so the
    /// indexer re-derives this language's files ([`crate::Indexer`]).
    ///
    /// Everything but the version constant is read from the code and the grammar,
    /// so a changed table row or a grammar upgrade needs no remembered bump. The
    /// constant covers a change to the chunker's own logic.
    pub fn extractor_stamp(self) -> Result<String> {
        let grammar = self.grammar()?;
        let mut h = blake3::Hasher::new();
        let mut feed = |part: &str| {
            h.update(part.as_bytes());
            h.update(&[0]);
        };
        feed(&EXTRACTOR_VERSION.to_string());
        feed(self.as_str());
        for row in self.spec() {
            feed(row.ts_kind);
            feed(row.kind.as_str());
            feed(&format!("{:?}", row.name));
        }
        for kind in self.call_kinds() {
            feed(kind);
        }
        feed(self.tags_query().unwrap_or(""));
        feed(grammar.name().unwrap_or(""));
        feed(&grammar.abi_version().to_string());
        if let Some(m) = grammar.metadata() {
            feed(&format!(
                "{}.{}.{}",
                m.major_version, m.minor_version, m.patch_version
            ));
        }
        for id in 0..grammar.node_kind_count() {
            let id = id as u16;
            feed(grammar.node_kind_for_id(id).unwrap_or(""));
            feed(if grammar.node_kind_is_named(id) {
                "n"
            } else {
                "a"
            });
        }
        for id in 1..=grammar.field_count() {
            feed(grammar.field_name_for_id(id as u16).unwrap_or(""));
        }
        Ok(h.finalize().to_hex().to_string())
    }

    /// Every language the indexer will chunk.
    pub fn all() -> &'static [Language] {
        &[
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::Markdown,
            Language::Go,
            Language::Java,
            Language::C,
            Language::Cpp,
            Language::Ruby,
            Language::CSharp,
            Language::Php,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grammar_loads() {
        // Fail loud: an absent or ABI-mismatched grammar must raise, and the only
        // way to know it does not is to install every one of them.
        for language in Language::all() {
            language
                .grammar()
                .unwrap_or_else(|e| panic!("{} grammar failed to load: {e}", language.as_str()));
        }
    }

    #[test]
    fn extractor_stamps_are_stable_and_distinct_per_language() {
        let mut seen = std::collections::BTreeSet::new();
        for language in Language::all() {
            let a = language.extractor_stamp().expect("stamp");
            assert_eq!(a, language.extractor_stamp().expect("stamp"), "stable");
            assert!(seen.insert(a), "{} shares a stamp", language.as_str());
        }
    }

    #[test]
    fn language_names_round_trip() {
        for language in Language::all() {
            assert_eq!(Language::parse(language.as_str()), Some(*language));
        }
    }

    #[test]
    fn unknown_extensions_are_not_guessed_at() {
        assert_eq!(Language::from_path("thing.bin"), None);
        assert_eq!(Language::from_path("Makefile"), None);
        assert_eq!(Language::from_path("src/lib.rs"), Some(Language::Rust));
        assert_eq!(Language::from_path("a/b/c.tsx"), Some(Language::Tsx));
    }

    #[test]
    fn spec_rows_are_unique_per_language() {
        // A duplicated tree-sitter kind in one table would make which rule wins
        // depend on ordering, which is exactly the kind of implicit behavior the
        // table exists to avoid.
        for language in Language::all() {
            let mut seen = std::collections::BTreeSet::new();
            for row in language.spec() {
                assert!(
                    seen.insert(row.ts_kind),
                    "{} lists `{}` twice",
                    language.as_str(),
                    row.ts_kind
                );
            }
        }
    }
}
