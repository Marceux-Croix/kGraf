//! The chunker: source text to a hierarchy of disjoint AST spans.
//!
//! Chunking follows codebase-memory-mcp: tree-sitter AST spans named by a
//! per-language specification table, **no sliding windows and no overlap**.
//!
//! "No overlap" needs saying precisely, because a method genuinely lies inside
//! a class. The property this chunker holds, and tests, is:
//!
//! 1. **Siblings are disjoint.** No two chunks at the same level share a byte.
//! 2. **A child is strictly contained in its parent.** Containment is the only
//!    permitted form of span intersection.
//! 3. **No byte range is emitted twice at the same level**, so the index never
//!    holds two copies of the same text and a slice never pays for one twice.
//!
//! That is what rules out sliding windows, which violate all three. A flat
//! partition would satisfy them too but would throw away the containment
//! structure the graph draws and the context slice walks.

use crate::error::{GraphError, Result};
use crate::lang::{Language, NameRule};
use kgraf_core::ids::stable_id;

/// What a graph node is.
///
/// Kinds are shared across languages where they mean the same thing, so a query
/// for functions finds them in Rust and Python alike, and language-specific
/// where they do not.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A whole file. Always present, and the root of that file's hierarchy.
    File,
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    /// A Rust `impl` block. Named for the type it implements.
    Impl,
    Module,
    Macro,
    TypeAlias,
    Constant,
    /// A Python definition wrapped in decorators.
    Decorated,
    /// A Markdown heading and the prose beneath it.
    Section,
}

impl NodeKind {
    /// The kind's stable name, for persistence, queries and display.
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::Struct => "struct",
            NodeKind::Enum => "enum",
            NodeKind::Trait => "trait",
            NodeKind::Interface => "interface",
            NodeKind::Class => "class",
            NodeKind::Impl => "impl",
            NodeKind::Module => "module",
            NodeKind::Macro => "macro",
            NodeKind::TypeAlias => "type_alias",
            NodeKind::Constant => "constant",
            NodeKind::Decorated => "decorated",
            NodeKind::Section => "section",
        }
    }

    /// Parse a persisted or queried kind name.
    pub fn parse(name: &str) -> Option<NodeKind> {
        Some(match name {
            "file" => NodeKind::File,
            "function" => NodeKind::Function,
            "method" => NodeKind::Method,
            "struct" => NodeKind::Struct,
            "enum" => NodeKind::Enum,
            "trait" => NodeKind::Trait,
            "interface" => NodeKind::Interface,
            "class" => NodeKind::Class,
            "impl" => NodeKind::Impl,
            "module" => NodeKind::Module,
            "macro" => NodeKind::Macro,
            "type_alias" => NodeKind::TypeAlias,
            "constant" => NodeKind::Constant,
            "decorated" => NodeKind::Decorated,
            "section" => NodeKind::Section,
            _ => return None,
        })
    }
}

/// One chunk: a location, never the text at that location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Deterministic node id, derived from path, kind, qualified name and the
    /// chunk's position among identically-named siblings. Stable across a
    /// reindex that does not change the element.
    pub id: String,
    /// The chunk's own name, as the language writes it.
    pub name: String,
    /// The name with its enclosing chunks' names prefixed, dot-separated.
    pub qualified_name: String,
    pub kind: NodeKind,
    /// Index into the chunk list of the enclosing chunk, or `None` for a chunk
    /// directly under the file.
    pub parent: Option<usize>,
    /// First line, 1-based and inclusive.
    pub start_line: u32,
    /// Last line, 1-based and inclusive.
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
    /// The element's signature line, for display and for a cheap slice that does
    /// not want the whole body.
    pub signature: String,
    /// Names called from inside this chunk but outside any of its children, so a
    /// call attributes to the innermost element that makes it.
    pub calls: Vec<String>,
    /// Whether this chunk's [`Chunk::id`] is stable across edits elsewhere in the file.
    ///
    /// **False means the id is unique within this snapshot and nothing more.** It happens
    /// when two siblings are indistinguishable by kind, qualified name and signature,
    /// two `impl A` blocks with no trait, or two overloads with identical signature lines.
    /// Such a pair has no stable identity to recover: the only thing separating them is
    /// their bodies, and a body-derived id changes the moment the body is edited, which is
    /// the one thing the id exists to survive.
    ///
    /// So the ambiguity is **declared** rather than papered over with a positional
    /// ordinal. An earlier version used the ordinal silently and it did not merely churn
    /// ids: inserting an earlier same-named sibling made one element's old id name a
    /// *different* element, so a lease or a cached position bound to code nobody asked
    /// for. Fail loud: the absent capability is stated; a consumer that needs stable
    /// identity, such as the write-lease registry, must refuse a chunk with this false and
    /// fall back to file granularity.
    pub identity_stable: bool,
}

/// Chunk one file's source.
///
/// `path` is used for node identity and error messages; the source is passed in
/// rather than read here so the caller controls IO.
pub fn chunk_source(path: &str, language: Language, source: &str) -> Result<Vec<Chunk>> {
    let grammar = language.grammar()?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|source| GraphError::Grammar {
            language: language.as_str(),
            source,
        })?;
    // tree-sitter returns None only on a timeout or cancellation, neither of
    // which is set here; treat it as a grammar failure rather than panicking.
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| GraphError::NotUtf8 {
            path: path.to_owned(),
        })?;

    let mut out: Vec<Chunk> = Vec::new();
    let call_kinds = language.call_kinds();

    if language == Language::Markdown {
        collect_markdown(path, source, tree.root_node(), &mut out);
    } else if let Some(query) = language.tags_query() {
        collect_tags(path, language, source, &tree, query, &mut out)?;
    } else {
        collect(path, language, source, tree.root_node(), None, "", &mut out);
    }

    // Calls are attributed after the hierarchy exists, so each call lands on the
    // innermost chunk containing it rather than on every ancestor.
    attribute_calls(source, &tree, call_kinds, &mut out);

    debug_assert!(
        invariants_hold(&out),
        "chunk invariants violated for {path}"
    );
    Ok(out)
}

/// Whether the three documented invariants hold over a chunk list.
///
/// Exposed for tests in this crate and in the indexer; cheap enough to assert in
/// debug builds on every file.
pub(crate) fn invariants_hold(chunks: &[Chunk]) -> bool {
    for (i, chunk) in chunks.iter().enumerate() {
        if chunk.start_byte > chunk.end_byte {
            return false;
        }
        // A child is strictly contained in its parent.
        if let Some(parent) = chunk.parent {
            if parent >= i {
                return false; // parents are emitted before children
            }
            let p = &chunks[parent];
            if chunk.start_byte < p.start_byte || chunk.end_byte > p.end_byte {
                return false;
            }
        }
        // Siblings are disjoint.
        for other in chunks.iter().take(i) {
            if other.parent != chunk.parent {
                continue;
            }
            let overlaps = chunk.start_byte < other.end_byte && other.start_byte < chunk.end_byte;
            if overlaps {
                return false;
            }
        }
    }
    true
}

fn collect(
    path: &str,
    language: Language,
    source: &str,
    node: tree_sitter::Node<'_>,
    parent: Option<usize>,
    prefix: &str,
    out: &mut Vec<Chunk>,
) {
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    drop(cursor);
    for child in children {
        let matched = language
            .spec()
            .iter()
            .find(|row| row.ts_kind == child.kind());
        match matched {
            Some(row) => {
                // A function whose enclosing element is an impl, trait or class
                // is a method, whatever the grammar calls it. tree-sitter's Rust
                // and Python grammars use one node type for both positions, and
                // collapsing that here is what lets `kind=method` mean the same
                // thing in a query across every language in the table.
                let kind = match (row.kind, parent.map(|i| out[i].kind)) {
                    (
                        NodeKind::Function,
                        Some(NodeKind::Impl | NodeKind::Trait | NodeKind::Class),
                    ) => NodeKind::Method,
                    (kind, _) => kind,
                };
                let name = extract_name(source, child, row.name).unwrap_or_else(|| {
                    format!("{}@{}", kind.as_str(), child.start_position().row + 1)
                });
                let qualified_name = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                // Disambiguate identically-qualified siblings. The discriminator is
                // derived from the element's own signature rather than from its position,
                // because a positional ordinal renumbers every later sibling when one is
                // inserted above, and the renumbering rebinds ids to different code rather
                // than merely invalidating them.
                // Disambiguate identically-qualified siblings. The discriminator is
                // derived from the element's own signature rather than from its position,
                // because a positional ordinal renumbers every later sibling when one is
                // inserted above, and that renumbering rebinds an id to *different* code
                // rather than merely invalidating it.
                //
                // Identity is hierarchical: a child's id is derived from its parent's id,
                // not from its parent's name. Two `impl` blocks for one type are separated
                // by their signatures, and their methods inherit that separation for free.
                // Keying a child on the parent's bare name instead would make
                // `Display for A`'s `fmt` and `Debug for A`'s `fmt` collide.
                let signature = signature_of(source, child);
                let (id, identity_stable) =
                    identity(path, out, parent, kind, &name, &qualified_name, &signature);
                let index = out.len();
                out.push(Chunk {
                    id,
                    name,
                    qualified_name: qualified_name.clone(),
                    kind,
                    parent,
                    start_line: child.start_position().row as u32 + 1,
                    end_line: child.end_position().row as u32 + 1,
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    signature,
                    calls: Vec::new(),
                    identity_stable,
                });
                collect(
                    path,
                    language,
                    source,
                    child,
                    Some(index),
                    &qualified_name,
                    out,
                );
            }
            None => {
                // Not a node type this language names. Descend through it so a
                // function inside a block or an `unsafe` wrapper is still found,
                // but do not emit the wrapper itself.
                collect(path, language, source, child, parent, prefix, out);
            }
        }
    }
}

/// A new chunk's id and whether it is stable: see the comments in [`collect`]
/// and [`Chunk::identity_stable`]. Shared by both tiers, so a tags-tier node gets
/// the same identity rules as a full-tier one.
fn identity(
    path: &str,
    out: &[Chunk],
    parent: Option<usize>,
    kind: NodeKind,
    name: &str,
    qualified_name: &str,
    signature: &str,
) -> (String, bool) {
    let parent_key = parent
        .map(|i| out[i].id.clone())
        .unwrap_or_else(|| path.to_owned());
    let sig_digest = stable_id("signature", &[signature]);
    let mut identity_stable = parent.map(|i| out[i].identity_stable).unwrap_or(true);
    let mut discriminator = sig_digest.clone();
    let candidate = stable_id("node", &[&parent_key, kind.as_str(), name, &sig_digest]);
    if out.iter().any(|c| c.id == candidate) {
        // The signature does not separate this element from a sibling, so there
        // is no stable identity available: the only thing left is the body, and
        // a body-derived id changes on the edit the id exists to survive. Fall
        // back to a positional ordinal to stay unique within the snapshot, and
        // declare the ambiguity.
        let ordinal = out
            .iter()
            .filter(|c| c.qualified_name == qualified_name && c.kind == kind)
            .count();
        discriminator = format!("{sig_digest}#{ordinal}");
        identity_stable = false;
    }
    let id = stable_id("node", &[&parent_key, kind.as_str(), name, &discriminator]);
    (id, identity_stable)
}

/// The tags tier's compiled queries, one per language, compiled once per process
/// (a compile is milliseconds; a walk chunks thousands of files).
fn tags_query(language: Language, source: &'static str) -> Result<&'static tree_sitter::Query> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<Language, &'static tree_sitter::Query>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(q) = cache.get(&language) {
        return Ok(q);
    }
    let grammar = language.grammar()?;
    let query = tree_sitter::Query::new(&grammar, source).map_err(|e| GraphError::TagsQuery {
        language: language.as_str(),
        reason: e.to_string(),
    })?;
    // Leaked on purpose: at most one per tags-tier language per process, held
    // for its life.
    let query: &'static tree_sitter::Query = Box::leak(Box::new(query));
    cache.insert(language, query);
    Ok(query)
}

/// The kGraf kind a tags capture (`definition.<kind>`) becomes, or `None` for
/// a kind that is not a structural element (a field, a constant's use).
fn tag_kind(tag: &str) -> Option<NodeKind> {
    Some(match tag {
        "function" => NodeKind::Function,
        "method" => NodeKind::Method,
        "class" => NodeKind::Class,
        "interface" => NodeKind::Interface,
        "module" => NodeKind::Module,
        "macro" => NodeKind::Macro,
        "type" => NodeKind::TypeAlias,
        "constant" => NodeKind::Constant,
        _ => return None,
    })
}

/// When two tags name the same span (Rust's upstream query tags a method as both
/// a method and a function), the more specific wins.
fn tag_rank(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Method => 3,
        NodeKind::Class | NodeKind::Interface | NodeKind::Module => 2,
        _ => 1,
    }
}

/// The tags tier: definitions and call references from the grammar crate's own
/// `TAGS_QUERY` (see `lang.rs`).
///
/// - A `definition.<kind>` capture is a node, named by the match's `name`
///   capture. Captures on the same span are deduplicated (the most specific
///   kind wins). A capture on a declarator (C and C++ tag the
///   `function_declarator`, not the definition) is widened to the node it
///   declares, so the body's calls land on the function.
/// - Nesting follows the spans: a definition inside another is its child, and a
///   function inside a class or interface is a method, as in the full tier. Syntax nodes either nest or are
///   disjoint, so the chunk invariants hold; a capture that would break them is dropped.
/// - A `reference.call` or `reference.send` capture is a call site, attributed
///   to the innermost definition containing its name.
/// - A pattern with an `is?` or `is-not?` predicate is skipped: those need the
///   local-scope analysis of the `tree-sitter-tags` runner, which this does not
///   run. Ruby's bare-identifier call pattern is the case: without the check every
///   local variable would be a call.
fn collect_tags(
    path: &str,
    language: Language,
    source: &str,
    tree: &tree_sitter::Tree,
    query_source: &'static str,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    use tree_sitter::StreamingIterator;
    let query = tags_query(language, query_source)?;
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut defs: Vec<(NodeKind, String, tree_sitter::Node<'_>)> = Vec::new();
    let mut refs: Vec<(usize, usize, String)> = Vec::new();
    let text = |n: tree_sitter::Node<'_>| source.get(n.start_byte()..n.end_byte()).map(str::trim);
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        if !query.property_predicates(m.pattern_index).is_empty() {
            continue;
        }
        let mut name = None;
        let mut def = None;
        let mut is_ref = false;
        for c in m.captures() {
            let capture = names[c.index as usize];
            if capture == "name" {
                name = Some(c.node);
            } else if let Some(tag) = capture.strip_prefix("definition.") {
                def = tag_kind(tag).map(|k| (k, c.node));
            } else if capture == "reference.call" || capture == "reference.send" {
                is_ref = true;
            }
        }
        let Some(name_node) = name else { continue };
        let Some(name_text) = text(name_node).filter(|t| {
            t.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
        }) else {
            continue;
        };
        if let Some((kind, node)) = def {
            let mut node = node;
            while let Some(parent) = node.parent() {
                if parent.child_by_field_name("declarator").map(|d| d.id()) == Some(node.id()) {
                    node = parent;
                } else {
                    break;
                }
            }
            defs.push((kind, name_text.to_owned(), node));
        } else if is_ref {
            refs.push((
                name_node.start_byte(),
                name_node.end_byte(),
                name_text.to_owned(),
            ));
        }
    }

    // One definition per span, the most specific kind; then outermost first.
    defs.sort_by(|a, b| {
        (a.2.start_byte(), std::cmp::Reverse(a.2.end_byte()))
            .cmp(&(b.2.start_byte(), std::cmp::Reverse(b.2.end_byte())))
            .then(tag_rank(b.0).cmp(&tag_rank(a.0)))
    });
    defs.dedup_by(|b, a| a.2.byte_range() == b.2.byte_range());

    let mut open: Vec<usize> = Vec::new();
    for (tag_kind, name, node) in defs {
        let (start, end) = (node.start_byte(), node.end_byte());
        while open.last().is_some_and(|&i| out[i].end_byte <= start) {
            open.pop();
        }
        let parent = open.last().copied();
        if let Some(p) = parent
            && end > out[p].end_byte
        {
            // Overlaps its would-be parent without nesting: not a syntax node
            // shape the invariants allow, so it is not emitted.
            continue;
        }
        let kind = match (tag_kind, parent.map(|i| out[i].kind)) {
            (
                NodeKind::Function,
                Some(NodeKind::Class | NodeKind::Interface | NodeKind::Trait | NodeKind::Impl),
            ) => NodeKind::Method,
            (k, _) => k,
        };
        let qualified_name = match parent {
            Some(p) => format!("{}.{name}", out[p].qualified_name),
            None => name.clone(),
        };
        let signature = signature_of(source, node);
        let (id, identity_stable) =
            identity(path, out, parent, kind, &name, &qualified_name, &signature);
        let index = out.len();
        out.push(Chunk {
            id,
            name,
            qualified_name,
            kind,
            parent,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            start_byte: start,
            end_byte: end,
            signature,
            calls: Vec::new(),
            identity_stable,
        });
        open.push(index);
    }

    for (start, end, name) in refs {
        let owner = out
            .iter()
            .enumerate()
            .rfind(|(_, c)| c.start_byte <= start && end <= c.end_byte)
            .map(|(i, _)| i);
        if let Some(i) = owner
            && !out[i].calls.contains(&name)
        {
            out[i].calls.push(name);
        }
    }
    for chunk in out.iter_mut() {
        chunk.calls.sort();
    }
    Ok(())
}

/// Markdown sections are built from the heading sequence rather than from the
/// AST's own nesting, because a heading's tree-sitter span is the heading line
/// alone. A section runs from its heading to the next heading of equal or
/// shallower depth, which is the span a reader would call "that section".
fn collect_markdown(path: &str, source: &str, root: tree_sitter::Node<'_>, out: &mut Vec<Chunk>) {
    struct Heading {
        depth: usize,
        name: String,
        start_byte: usize,
        start_line: u32,
        signature: String,
    }

    let mut headings: Vec<Heading> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let is_heading = matches!(node.kind(), "atx_heading" | "setext_heading");
        if is_heading {
            let text = source
                .get(node.start_byte()..node.end_byte())
                .unwrap_or_default();
            let line = text.lines().next().unwrap_or_default();
            let depth = if node.kind() == "setext_heading" {
                // A setext heading is underlined with `=` for depth 1, `-` for 2.
                if text
                    .lines()
                    .nth(1)
                    .is_some_and(|l| l.trim_start().starts_with('='))
                {
                    1
                } else {
                    2
                }
            } else {
                line.chars().take_while(|c| *c == '#').count().max(1)
            };
            headings.push(Heading {
                depth,
                name: line.trim_start_matches('#').trim().to_owned(),
                start_byte: node.start_byte(),
                start_line: node.start_position().row as u32 + 1,
                signature: line.trim().to_owned(),
            });
        }
        let mut cursor = node.walk();
        // Push in reverse so the traversal visits the document in order.
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    headings.sort_by_key(|h| h.start_byte);

    let total_lines = source.lines().count().max(1) as u32;
    // `open` holds (chunk index, depth) for sections still accepting children.
    let mut open: Vec<(usize, usize)> = Vec::new();
    for (i, heading) in headings.iter().enumerate() {
        while open
            .last()
            .is_some_and(|(_, depth)| *depth >= heading.depth)
        {
            open.pop();
        }
        let parent = open.last().map(|(index, _)| *index);
        let prefix = parent
            .map(|p| out[p].qualified_name.clone())
            .unwrap_or_default();
        let qualified_name = if prefix.is_empty() {
            heading.name.clone()
        } else {
            format!("{prefix}.{}", heading.name)
        };
        // A heading path is normally unique in a document, so the signature digest is
        // the discriminator. Where two sections share a full heading path there is no
        // stable identity to recover, and the positional fallback is declared rather than
        // silent; see `Chunk::identity_stable`.
        let sig_digest = stable_id("signature", &[&heading.signature]);
        let ordinal = out
            .iter()
            .filter(|c| c.qualified_name == qualified_name)
            .count();
        let section_stable = ordinal == 0;
        let discriminator = if section_stable {
            sig_digest
        } else {
            format!("{sig_digest}#{ordinal}")
        };
        let end_byte = headings
            .iter()
            .skip(i + 1)
            .find(|next| next.depth <= heading.depth)
            .map(|next| next.start_byte)
            .unwrap_or(source.len());
        let end_line = headings
            .iter()
            .skip(i + 1)
            .find(|next| next.depth <= heading.depth)
            .map(|next| next.start_line.saturating_sub(1).max(heading.start_line))
            .unwrap_or(total_lines);
        let index = out.len();
        out.push(Chunk {
            id: stable_id(
                "node",
                &[
                    path,
                    NodeKind::Section.as_str(),
                    &qualified_name,
                    &discriminator,
                ],
            ),
            name: heading.name.clone(),
            qualified_name,
            kind: NodeKind::Section,
            parent,
            start_line: heading.start_line,
            end_line,
            start_byte: heading.start_byte,
            end_byte,
            signature: heading.signature.clone(),
            calls: Vec::new(),
            identity_stable: section_stable,
        });
        open.push((index, heading.depth));
    }

    // A nested section's span must not exceed its parent's; clamp rather than
    // trust, so the invariant holds even on malformed heading sequences.
    for i in 0..out.len() {
        if let Some(parent) = out[i].parent {
            let (pe, pel) = (out[parent].end_byte, out[parent].end_line);
            out[i].end_byte = out[i].end_byte.min(pe);
            out[i].end_line = out[i].end_line.min(pel);
        }
    }
}

fn extract_name(source: &str, node: tree_sitter::Node<'_>, rule: NameRule) -> Option<String> {
    let text =
        |n: tree_sitter::Node<'_>| source.get(n.start_byte()..n.end_byte()).map(str::to_owned);
    match rule {
        NameRule::NameField => node.child_by_field_name("name").and_then(text),
        NameRule::TypeField => node.child_by_field_name("type").and_then(text),
        NameRule::FirstIdentifier => {
            let mut cursor = node.walk();
            // One level down is enough: a decorated definition's target is a
            // direct child, and descending further would pick up a parameter.
            node.children(&mut cursor)
                .find_map(|child| child.child_by_field_name("name"))
                .and_then(text)
        }
        NameRule::HeadingText => text(node).map(|t| {
            t.lines()
                .next()
                .unwrap_or_default()
                .trim_start_matches('#')
                .trim()
                .to_owned()
        }),
    }
}

/// The element's declaration, excluding its body.
///
/// Taken as the source from the element's start to the start of its `body` child where the
/// grammar exposes one, falling back to the first line where it does not, Markdown
/// headings have no body, for instance.
///
/// **This must exclude the body, and the reason is not cosmetic.** The signature is the
/// discriminator that separates identically-named siblings (see `Chunk::identity_stable`),
/// so a body-sensitive signature makes a node's id change whenever its body is edited,
/// which is the one edit the id exists to survive. An earlier version returned the first
/// line, which for a one-line function such as `fn beta() {}` includes the whole body, and
/// the test asserting id survival across a body edit is what caught it.
fn signature_of(source: &str, node: tree_sitter::Node<'_>) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or_else(|| {
            let text = source
                .get(node.start_byte()..node.end_byte())
                .unwrap_or_default();
            node.start_byte() + text.lines().next().map(str::len).unwrap_or(0)
        });
    let text = source.get(node.start_byte()..end).unwrap_or_default();
    text.trim().trim_end_matches('{').trim_end().to_owned()
}

/// Attach each call site to the innermost chunk that contains it.
fn attribute_calls(source: &str, tree: &tree_sitter::Tree, call_kinds: &[&str], out: &mut [Chunk]) {
    if call_kinds.is_empty() {
        return;
    }
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if call_kinds.contains(&node.kind())
            && let Some(name) = callee_name(source, node)
        {
            // The innermost containing chunk is the last one in emission order
            // whose span contains the call, since parents precede children.
            let owner = out
                .iter()
                .enumerate()
                .rfind(|(_, c)| c.start_byte <= node.start_byte() && node.end_byte() <= c.end_byte)
                .map(|(i, _)| i);
            if let Some(i) = owner
                && !out[i].calls.contains(&name)
            {
                out[i].calls.push(name);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    for chunk in out.iter_mut() {
        chunk.calls.sort();
    }
}

/// The bare name being called, with any receiver or path qualifier stripped.
///
/// Resolution is by name only and is deliberately shallow: it is good enough for
/// navigation and for a context slice, which is all the call graph is for after
/// decision 0020 took the collision-prediction job away from it.
fn callee_name(source: &str, node: tree_sitter::Node<'_>) -> Option<String> {
    let callee = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("macro"))
        .or_else(|| node.child_by_field_name("constructor"))
        .or_else(|| node.named_child(0))?;
    let text = source.get(callee.start_byte()..callee.end_byte())?;
    let bare = text
        .rsplit("::")
        .next()
        .and_then(|t| t.rsplit('.').next())
        // PHP's namespace separator (`\App\helper()`); no other indexed language
        // has a backslash in a callee.
        .and_then(|t| t.rsplit('\\').next())
        .unwrap_or(text)
        .trim();
    if bare.is_empty()
        || !bare
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return None;
    }
    Some(bare.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_functions_and_impls_are_chunked_with_containment() {
        let source = r#"
struct Widget { size: u32 }

impl Widget {
    fn new() -> Self { Widget { size: 0 } }
    fn grow(&mut self) { self.size += 1; }
}

fn main() { let mut w = Widget::new(); w.grow(); }
"#;
        let chunks = chunk_source("src/main.rs", Language::Rust, source).expect("chunk");
        let names: Vec<_> = chunks.iter().map(|c| c.qualified_name.as_str()).collect();
        assert!(names.contains(&"Widget"), "got {names:?}");
        assert!(names.contains(&"Widget.new"), "got {names:?}");
        assert!(names.contains(&"Widget.grow"), "got {names:?}");
        assert!(names.contains(&"main"), "got {names:?}");

        // `Widget.new` is a child of the impl block, not a sibling of it.
        let new_index = chunks
            .iter()
            .position(|c| c.qualified_name == "Widget.new")
            .expect("new");
        let parent = chunks[new_index].parent.expect("new has a parent");
        assert_eq!(chunks[parent].kind, NodeKind::Impl);
    }

    #[test]
    fn chunk_invariants_hold_on_every_language() {
        let cases: &[(&str, Language, &str)] = &[
            (
                "a.rs",
                Language::Rust,
                "mod inner { fn a() {} fn b() {} }\nfn c() {}\n",
            ),
            (
                "a.py",
                Language::Python,
                "class C:\n    def m(self):\n        pass\n\n@deco\ndef f():\n    pass\n",
            ),
            (
                "a.ts",
                Language::TypeScript,
                "interface I { x: number }\nclass C { m(): void {} }\nfunction f(): void {}\n",
            ),
            (
                "a.js",
                Language::JavaScript,
                "class C { m() {} }\nfunction f() {}\n",
            ),
            (
                "a.md",
                Language::Markdown,
                "# Top\n\ntext\n\n## Sub\n\nmore\n\n## Sub2\n\nend\n\n# Other\n",
            ),
        ];
        for (path, language, source) in cases {
            let chunks = chunk_source(path, *language, source).expect("chunk");
            assert!(!chunks.is_empty(), "{path} produced no chunks");
            assert!(
                invariants_hold(&chunks),
                "{path} violated the chunk invariants: {chunks:#?}"
            );
        }
    }

    #[test]
    fn no_two_chunks_share_an_id() {
        // Identically-named siblings are the case that breaks a naive id scheme:
        // two `impl` blocks for the same type, or two same-named methods.
        let source = "impl A { fn f() {} }\nimpl A { fn f() {} }\n";
        let chunks = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        let mut ids: Vec<_> = chunks.iter().map(|c| c.id.clone()).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate node ids in {chunks:#?}");
    }

    #[test]
    fn chunk_ids_are_stable_across_reparsing() {
        let source = "fn alpha() {}\nfn beta() {}\n";
        let first = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        let second = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        assert_eq!(
            first.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            second.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_chunk_id_survives_an_edit_elsewhere_in_the_file() {
        // This is what makes an incremental reindex incremental and what keeps a
        // node's cached layout position: editing `beta` must not renumber
        // `alpha`.
        let before =
            chunk_source("a.rs", Language::Rust, "fn alpha() {}\nfn beta() {}\n").expect("chunk");
        let after = chunk_source(
            "a.rs",
            Language::Rust,
            "fn alpha() {}\nfn beta() { let x = 1; }\n",
        )
        .expect("chunk");
        let id_of = |cs: &[Chunk], name: &str| {
            cs.iter()
                .find(|c| c.name == name)
                .map(|c| c.id.clone())
                .expect("present")
        };
        assert_eq!(id_of(&before, "alpha"), id_of(&after, "alpha"));
        assert_eq!(id_of(&before, "beta"), id_of(&after, "beta"));
    }

    #[test]
    fn a_trait_impl_and_its_methods_get_stable_identities() {
        // The free win from taking the signature rather than the first line: the trait is
        // in the declaration, so two impls of one type separate, and because identity is
        // hierarchical their methods inherit that separation instead of colliding on
        // `A.fmt`.
        let source = "impl Display for A { fn fmt() {} }\nimpl Debug for A { fn fmt() {} }\n";
        let chunks = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        assert!(
            chunks.iter().all(|c| c.identity_stable),
            "trait impls and their methods must all be identifiable: {chunks:#?}"
        );
        let ids: std::collections::BTreeSet<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids.len(), chunks.len(), "ids must be distinct");
    }

    #[test]
    fn genuinely_indistinguishable_siblings_are_declared_not_silently_renumbered() {
        // Two `impl A` blocks with identical signatures differ only in their bodies, and a
        // body-derived id changes on the edit an id exists to survive. So there is no
        // stable identity here, and the honest answer is to say so: a positional ordinal
        // keeps the snapshot unique and `identity_stable` warns every consumer off.
        let source = "impl A { fn go() { one(); } }\nimpl A { fn go() { two(); } }\n";
        let chunks = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        let impls: Vec<&Chunk> = chunks.iter().filter(|c| c.kind == NodeKind::Impl).collect();
        assert_eq!(impls.len(), 2);
        assert!(
            impls[0].identity_stable,
            "the first is identifiable by signature"
        );
        assert!(
            !impls[1].identity_stable,
            "the second is not, and must say so"
        );
        // The ambiguity is inherited, because a child of an unidentifiable parent cannot
        // itself be identified.
        let second_method = chunks
            .iter()
            .rfind(|c| c.kind == NodeKind::Method)
            .expect("method");
        assert!(!second_method.identity_stable);
    }

    #[test]
    fn inserting_a_sibling_does_not_rebind_an_identifiable_sibling_s_id() {
        // The defect this replaced. With a positional ordinal, inserting an earlier
        // same-named element did not merely churn ids: the element that had been ordinal 0
        // became ordinal 1, so its old id named *different code*, and a lease or cached
        // position bound to something nobody asked for.
        let before =
            chunk_source("a.rs", Language::Rust, "fn alpha() {}\nfn beta() {}\n").expect("chunk");
        let after = chunk_source(
            "a.rs",
            Language::Rust,
            "fn inserted() {}\nfn alpha() {}\nfn beta() {}\n",
        )
        .expect("chunk");
        let id_of = |cs: &[Chunk], name: &str| {
            cs.iter()
                .find(|c| c.name == name)
                .map(|c| c.id.clone())
                .expect("present")
        };
        assert_eq!(id_of(&before, "alpha"), id_of(&after, "alpha"));
        assert_eq!(id_of(&before, "beta"), id_of(&after, "beta"));
    }

    #[test]
    fn a_signature_excludes_the_body_so_a_body_edit_does_not_move_the_id() {
        // The signature is the discriminator, so a body-sensitive signature would make
        // every id churn on every edit. `fn beta() {}` is the case that catches it: its
        // first line is its whole body.
        let before = chunk_source("a.rs", Language::Rust, "fn beta() {}\n").expect("chunk");
        let after =
            chunk_source("a.rs", Language::Rust, "fn beta() { let x = 1; }\n").expect("chunk");
        assert_eq!(before[0].signature, "fn beta()");
        assert_eq!(before[0].signature, after[0].signature);
        assert_eq!(before[0].id, after[0].id);
    }

    #[test]
    fn calls_attribute_to_the_innermost_element() {
        let source = "fn outer() { helper(); }\nfn helper() {}\n";
        let chunks = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        let outer = chunks.iter().find(|c| c.name == "outer").expect("outer");
        assert!(
            outer.calls.contains(&"helper".to_owned()),
            "{:?}",
            outer.calls
        );
        let helper = chunks.iter().find(|c| c.name == "helper").expect("helper");
        assert!(helper.calls.is_empty(), "{:?}", helper.calls);
    }

    #[test]
    fn markdown_sections_nest_by_heading_depth() {
        let source = "# Top\n\nintro\n\n## Sub\n\nbody\n\n# Other\n\ntail\n";
        let chunks = chunk_source("a.md", Language::Markdown, source).expect("chunk");
        let sub = chunks.iter().find(|c| c.name == "Sub").expect("Sub");
        let parent = sub.parent.expect("Sub is nested");
        assert_eq!(chunks[parent].name, "Top");
        let other = chunks.iter().find(|c| c.name == "Other").expect("Other");
        assert_eq!(other.parent, None);
        // `Top` must stop where `Other` begins rather than swallowing it.
        let top = chunks.iter().find(|c| c.name == "Top").expect("Top");
        assert!(top.end_byte <= other.start_byte, "Top swallowed Other");
    }

    #[test]
    fn there_are_no_sliding_windows() {
        // A sliding-window chunker on this input would emit overlapping spans
        // covering the same bytes more than once. Every chunk here must be
        // either disjoint from or strictly nested in every other.
        let source = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let chunks = chunk_source("a.rs", Language::Rust, source).expect("chunk");
        for (i, x) in chunks.iter().enumerate() {
            for y in chunks.iter().skip(i + 1) {
                let disjoint = x.end_byte <= y.start_byte || y.end_byte <= x.start_byte;
                let nested = (x.start_byte <= y.start_byte && y.end_byte <= x.end_byte)
                    || (y.start_byte <= x.start_byte && x.end_byte <= y.end_byte);
                assert!(disjoint || nested, "{x:?} overlaps {y:?} without nesting");
            }
        }
    }
}
