//! A contract instruction that changes a public shape the repository defines.
//!
//! Existing public items are the interface: other code is written against a
//! type's shape, a trait method's signature, a function's signature and an enum
//! variant's fields. An instruction that restates one of them differently
//! breaks every caller, and the compile error shows in a file the contract
//! does not write (round 7: `Act::act` gained a parameter and crate b stopped
//! building).
//!
//! The check is structural. It reads the public declarations of the Rust files
//! the contract writes with tree-sitter ([`declarations`]), finds the code the
//! instruction shows (backtick spans, fenced blocks, and `struct`, `enum`,
//! `trait`, `fn` and `impl` items written bare), parses that code the same way,
//! and compares the two by shape:
//!
//! - a struct: its kind (unit, tuple, braced) and its public fields;
//! - an enum: the fields of each variant the instruction names;
//! - a trait method, an inherent method or a function: the signature without
//!   parameter names (types, `self` receiver, generics, modifiers, return).
//!
//! Anything the repository does not define is an addition and is not reported.
//! So is a private item: nothing outside its module depends on it. A found
//! change is a notice, never a refusal, because the user's instruction may ask
//! for it.
//!
//! The pattern fallbacks run only where the structure cannot be read:
//! a unit struct given fields as `Name { ... }` text that is not an item
//! ([`crate::plan::defines_with_fields`]), and a sentence that names a public
//! item in backticks together with a verb of change ("change the trait", "add
//! a parameter to `run`"). A Rust file that does not parse cleanly is still
//! read: tree-sitter recovers around the error.

use crate::plan::{ContractSpec, defines_with_fields};
use kgraf_graph::Language;
use std::collections::BTreeSet;
use tree_sitter::{Node, Parser};

/// A struct or variant body.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fields {
    Unit,
    Tuple(Vec<Field>),
    Named(Vec<Field>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Field {
    /// Empty for a tuple field.
    name: String,
    /// Type text without whitespace.
    ty: String,
    public: bool,
}

/// One declaration of a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Decl {
    Struct {
        name: String,
        public: bool,
        fields: Fields,
    },
    Enum {
        name: String,
        public: bool,
        variants: Vec<(String, Fields)>,
    },
    /// A function, an inherent method or a trait method. `owner` is the trait
    /// or type; `None` for a free function and for a bare signature the
    /// instruction shows without its trait.
    Callable {
        owner: Option<String>,
        name: String,
        public: bool,
        sig: String,
    },
}

/// The owner a bare signature is wrapped in to be parsed. Never matched.
const BARE: &str = "__Bare";

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn field_text(node: Node<'_>, field: &str, src: &str) -> String {
    node.child_by_field_name(field)
        .map(|n| text(n, src).to_owned())
        .unwrap_or_default()
}

fn has_visibility(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| c.kind() == "visibility_modifier")
}

fn fields_of(body: Option<Node<'_>>, src: &str) -> Fields {
    let Some(body) = body else {
        return Fields::Unit;
    };
    let mut out = Vec::new();
    match body.kind() {
        "field_declaration_list" => {
            let mut cursor = body.walk();
            for f in body.children(&mut cursor) {
                if f.kind() == "field_declaration" {
                    out.push(Field {
                        name: field_text(f, "name", src),
                        ty: squash(&field_text(f, "type", src)),
                        public: has_visibility(f),
                    });
                }
            }
            Fields::Named(out)
        }
        "ordered_field_declaration_list" => {
            let mut public = false;
            let mut walk = body.walk();
            if walk.goto_first_child() {
                loop {
                    let child = walk.node();
                    if child.kind() == "visibility_modifier" {
                        public = true;
                    } else if walk.field_name() == Some("type") {
                        out.push(Field {
                            name: String::new(),
                            ty: squash(text(child, src)),
                            public,
                        });
                        public = false;
                    }
                    if !walk.goto_next_sibling() {
                        break;
                    }
                }
            }
            Fields::Tuple(out)
        }
        _ => Fields::Unit,
    }
}

/// The signature of a function item, without parameter names.
fn signature(node: Node<'_>, src: &str) -> String {
    let mut cursor = node.walk();
    let modifiers = node
        .children(&mut cursor)
        .find(|c| c.kind() == "function_modifiers")
        .map(|m| squash(text(m, src)))
        .unwrap_or_default();
    let mut params = Vec::new();
    if let Some(list) = node.child_by_field_name("parameters") {
        let mut c = list.walk();
        for p in list.named_children(&mut c) {
            match p.kind() {
                "parameter" => params.push(squash(&field_text(p, "type", src))),
                "line_comment" | "block_comment" | "attribute_item" => {}
                _ => params.push(squash(text(p, src))),
            }
        }
    }
    format!(
        "fn{modifiers} {}{}({})->{}",
        field_text(node, "name", src),
        squash(&field_text(node, "type_parameters", src)),
        params.join(","),
        squash(&field_text(node, "return_type", src)),
    )
}

fn last_segment(path: &str) -> String {
    let base = path.split('<').next().unwrap_or(path);
    base.rsplit("::").next().unwrap_or(base).trim().to_owned()
}

/// Collect the declarations under `node`. `existing` skips the methods of
/// `impl Trait for Type`: those implement a trait the repository defines
/// elsewhere, and the trait is the shape.
fn collect(node: Node<'_>, src: &str, existing: bool, out: &mut Vec<Decl>) {
    let mut cursor = node.walk();
    for item in node.named_children(&mut cursor) {
        match item.kind() {
            "struct_item" => out.push(Decl::Struct {
                name: field_text(item, "name", src),
                public: has_visibility(item),
                fields: fields_of(item.child_by_field_name("body"), src),
            }),
            "enum_item" => {
                let mut variants = Vec::new();
                if let Some(list) = item.child_by_field_name("body") {
                    let mut c = list.walk();
                    for v in list.named_children(&mut c) {
                        if v.kind() == "enum_variant" {
                            variants.push((
                                field_text(v, "name", src),
                                fields_of(v.child_by_field_name("body"), src),
                            ));
                        }
                    }
                }
                out.push(Decl::Enum {
                    name: field_text(item, "name", src),
                    public: has_visibility(item),
                    variants,
                });
            }
            "function_item" | "function_signature_item" => out.push(Decl::Callable {
                owner: None,
                name: field_text(item, "name", src),
                public: has_visibility(item),
                sig: signature(item, src),
            }),
            "trait_item" => {
                let owner = field_text(item, "name", src);
                let public = has_visibility(item);
                methods_of(item, src, &owner, public, false, out);
            }
            "impl_item" => {
                let ty = last_segment(&field_text(item, "type", src));
                match item.child_by_field_name("trait") {
                    // The trait's own methods are the shape; in the
                    // instruction a restated impl method is a restated
                    // trait method.
                    Some(tr) if !existing => {
                        methods_of(item, src, &last_segment(text(tr, src)), true, true, out);
                    }
                    Some(_) => {}
                    None => methods_of(item, src, &ty, false, false, out),
                }
            }
            "mod_item" => {
                if let Some(body) = item.child_by_field_name("body") {
                    collect(body, src, existing, out);
                }
            }
            _ => {}
        }
    }
}

/// The methods in an `impl` or `trait` body. In a trait (`always_public` for
/// a trait impl in the instruction) a method has no visibility of its own: it
/// is as public as the trait.
fn methods_of(
    item: Node<'_>,
    src: &str,
    owner: &str,
    trait_public: bool,
    always_public: bool,
    out: &mut Vec<Decl>,
) {
    let Some(body) = item.child_by_field_name("body") else {
        return;
    };
    let in_trait = item.kind() == "trait_item" || always_public;
    let mut c = body.walk();
    for m in body.named_children(&mut c) {
        if matches!(m.kind(), "function_item" | "function_signature_item") {
            out.push(Decl::Callable {
                owner: Some(owner.to_owned()),
                name: field_text(m, "name", src),
                public: if in_trait {
                    trait_public
                } else {
                    has_visibility(m)
                },
                sig: signature(m, src),
            });
        }
    }
}

/// The declarations of Rust source, `None` when the grammar is unavailable.
fn declarations(src: &str, existing: bool) -> Option<Vec<Decl>> {
    let grammar = Language::Rust.grammar().ok()?;
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(src, None)?;
    let mut out = Vec::new();
    collect(tree.root_node(), src, existing, &mut out);
    Some(out)
}

/// The declarations of a snippet the instruction shows. The snippet may be a
/// whole item, or a bare method signature with or without its `;`: it is read
/// as written, then with a `;`, then inside a trait body. Only a parse with no
/// error counts, so prose in backticks yields nothing.
fn snippet_declarations(snippet: &str) -> Vec<Decl> {
    let Some(grammar) = Language::Rust.grammar().ok() else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let s = snippet.trim().trim_end_matches([',', '.']);
    if s.is_empty() {
        return Vec::new();
    }
    let tries = [
        s.to_owned(),
        format!("{s};"),
        format!("trait {BARE} {{ {s} }}"),
        format!("trait {BARE} {{ {s}; }}"),
    ];
    for src in tries {
        let Some(tree) = parser.parse(&src, None) else {
            continue;
        };
        if tree.root_node().has_error() {
            continue;
        }
        let mut out = Vec::new();
        collect(tree.root_node(), &src, false, &mut out);
        for d in &mut out {
            if let Decl::Callable { owner, .. } = d
                && owner.as_deref() == Some(BARE)
            {
                *owner = None;
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

// ------------------------------------------------------------------ the code an instruction shows

fn push_unique(out: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if !s.is_empty() && !out.iter().any(|o| o == s) {
        out.push(s.to_owned());
    }
}

/// The text inside backtick spans and fences.
fn backtick_spans(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
        let open = "`".repeat(run);
        let body_start = i + run;
        match text[body_start..].find(&open) {
            Some(rel) => {
                let mut body = &text[body_start..body_start + rel];
                // A fence's language tag is on its first line.
                if run >= 3
                    && let Some((first, rest)) = body.split_once('\n')
                    && first.trim().chars().all(|c| c.is_ascii_alphanumeric())
                {
                    body = rest;
                }
                out.push(body.to_owned());
                i = body_start + rel + run;
            }
            None => i = body_start,
        }
    }
    out
}

/// The item that starts at `start`: to the `;` at depth 0, to the close of the
/// first braced body, or to the end of the line when nothing is open.
fn item_from(text: &str, start: usize) -> &str {
    let mut depth = 0i32;
    let mut opened_brace = false;
    for (off, ch) in text[start..].char_indices() {
        match ch {
            '{' => {
                depth += 1;
                opened_brace = true;
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '}' => {
                depth -= 1;
                if depth <= 0 && opened_brace {
                    return &text[start..start + off + 1];
                }
            }
            ';' if depth <= 0 => return &text[start..start + off + 1],
            '\n' | '`' if depth <= 0 => return &text[start..start + off],
            _ => {}
        }
    }
    &text[start..]
}

fn is_word_boundary(text: &str, at: usize, len: usize) -> bool {
    let b = text.as_bytes();
    let before = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_');
    let after =
        at + len >= b.len() || !(b[at + len].is_ascii_alphanumeric() || b[at + len] == b'_');
    before && after
}

/// Every piece of the instruction that may be Rust: each backtick span, and
/// each item written bare or inside a span (`pub struct ..`, `fn ..`).
fn code_candidates(instruction: &str) -> Vec<String> {
    let mut out = Vec::new();
    for span in backtick_spans(instruction) {
        push_unique(&mut out, &span);
        scan_items(&span, &mut out);
    }
    scan_items(instruction, &mut out);
    out
}

fn scan_items(text: &str, out: &mut Vec<String>) {
    for kw in ["struct", "enum", "trait", "fn", "impl"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(kw) {
            let at = from + rel;
            from = at + kw.len();
            if !is_word_boundary(text, at, kw.len()) {
                continue;
            }
            // Include a `pub` / `pub(crate)` before the keyword.
            let head = text[..at].trim_end();
            let start = match head.rfind(|c: char| c.is_whitespace()) {
                Some(w) if head[w + 1..].starts_with("pub") => w + 1,
                None if head.starts_with("pub") => 0,
                _ => at,
            };
            push_unique(out, item_from(text, start));
        }
    }
}

// --------------------------------------------------------------------------- comparison

fn exposed(fields: &[Field]) -> bool {
    fields.is_empty() || fields.iter().any(|f| f.public)
}

fn all_public(fields: &[Field]) -> bool {
    fields.iter().all(|f| f.public)
}

fn field_list(f: &Fields) -> &[Field] {
    match f {
        Fields::Unit => &[],
        Fields::Tuple(v) | Fields::Named(v) => v,
    }
}

/// Whether restating a struct (or variant) body as `new` changes what code
/// outside the declaration depends on. `variant`: every field of a variant is
/// public, so any difference counts.
fn fields_changed(old: &Fields, new: &Fields, variant: bool) -> bool {
    let (o, n) = (field_list(old), field_list(new));
    match (old, new) {
        (Fields::Unit, Fields::Unit) => false,
        // A unit struct is a value; fields make it a constructor.
        (Fields::Unit, _) => true,
        (_, Fields::Unit) => variant || exposed(o) && !o.is_empty(),
        (Fields::Named(_), Fields::Named(_)) => {
            let public = |f: &Field| variant || f.public;
            for f in o.iter().filter(|f| public(f)) {
                match n.iter().find(|g| g.name == f.name) {
                    Some(g) if g.ty == f.ty => {}
                    _ => return true,
                }
            }
            let added = n.iter().any(|g| !o.iter().any(|f| f.name == g.name));
            added && (variant || all_public(o))
        }
        (Fields::Tuple(_), Fields::Tuple(_)) => {
            for (i, f) in o.iter().enumerate().filter(|(_, f)| variant || f.public) {
                match n.get(i) {
                    Some(g) if g.ty == f.ty => {}
                    _ => return true,
                }
            }
            o.len() != n.len() && (variant || all_public(o))
        }
        // The kind changed: braces for a tuple or the reverse.
        _ => variant || exposed(o),
    }
}

fn describe(fields: &Fields) -> String {
    let list = |v: &[Field]| {
        v.iter()
            .map(|f| {
                let vis = if f.public { "pub " } else { "" };
                if f.name.is_empty() {
                    format!("{vis}{}", f.ty)
                } else {
                    format!("{vis}{}: {}", f.name, f.ty)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    match fields {
        Fields::Unit => "a unit struct".to_owned(),
        Fields::Tuple(v) => format!("({})", list(v)),
        Fields::Named(v) => format!("{{ {} }}", list(v)),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A unit struct given fields.
    Unit,
    /// A restated signature or body that differs.
    Shape,
    /// A sentence that asks for a change.
    Prose,
}

/// One shape change the instruction makes.
struct Change {
    kind: Kind,
    /// The item, as the instruction names it (`Act::act`, `Thing`).
    item: String,
    /// What the instruction gives it.
    new: String,
    /// What the repository defines.
    old: String,
}

fn compare(existing: &[Decl], restated: &[Decl]) -> Vec<Change> {
    let mut out = Vec::new();
    for n in restated {
        match n {
            Decl::Struct { name, fields, .. } => {
                for e in existing {
                    if let Decl::Struct {
                        name: en,
                        public: true,
                        fields: ef,
                    } = e
                        && en == name
                        && fields_changed(ef, fields, false)
                    {
                        out.push(Change {
                            kind: if *ef == Fields::Unit {
                                Kind::Unit
                            } else {
                                Kind::Shape
                            },
                            item: name.clone(),
                            new: format!("{name} {}", describe(fields)),
                            old: format!("{name} {}", describe(ef)),
                        });
                    }
                }
            }
            Decl::Enum { name, variants, .. } => {
                for e in existing {
                    let Decl::Enum {
                        name: en,
                        public: true,
                        variants: ev,
                    } = e
                    else {
                        continue;
                    };
                    if en != name {
                        continue;
                    }
                    for (vn, vf) in variants {
                        if let Some((_, of)) = ev.iter().find(|(x, _)| x == vn)
                            && fields_changed(of, vf, true)
                        {
                            out.push(Change {
                                kind: Kind::Shape,
                                item: format!("{name}::{vn}"),
                                new: format!("{vn} {}", describe(vf)),
                                old: format!("{vn} {}", describe(of)),
                            });
                        }
                    }
                }
            }
            Decl::Callable {
                owner, name, sig, ..
            } => {
                let same: Vec<&str> = existing
                    .iter()
                    .filter_map(|e| match e {
                        Decl::Callable {
                            owner: eo,
                            name: en,
                            public: true,
                            sig: es,
                        } if en == name && (owner.is_none() || owner == eo) => Some(es.as_str()),
                        _ => None,
                    })
                    .collect();
                if let Some(first) = same.first()
                    && !same.contains(&sig.as_str())
                    // The trait and the method inside it are one change.
                    && !out.iter().any(|c: &Change| c.new == *sig && c.old == **first)
                {
                    out.push(Change {
                        kind: Kind::Shape,
                        item: match owner {
                            Some(o) => format!("{o}::{name}"),
                            None => name.clone(),
                        },
                        new: sig.clone(),
                        old: (*first).to_owned(),
                    });
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------- the pattern fallbacks

/// Verbs that change something that exists.
const CHANGE_WORDS: [&str; 12] = [
    "change", "modify", "alter", "rename", "remove", "replace", "redefine", "drop", "widen",
    "narrow", "extend", "rewrite",
];
/// Phrases that add a parameter or field to something that exists.
const ADD_PHRASES: [&str; 6] = [
    "add a parameter",
    "add an argument",
    "add a field",
    "adds a parameter",
    "adds an argument",
    "adds a field",
];
/// A sentence with one of these keeps things as they are.
const KEEP_WORDS: [&str; 9] = [
    "not ",
    "never",
    "without",
    "keep",
    "unchanged",
    "as is",
    "n't",
    "stays",
    "preserve",
];

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    let b = text.as_bytes();
    for (i, ch) in text.char_indices() {
        let end = ch == '\n'
            || ch == ';'
            || (ch == '.' && b.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()));
        if end {
            out.push(&text[from..i]);
            from = i + 1;
        }
    }
    out.push(&text[from..]);
    out
}

/// The leading path of a backtick span: `Act::act(&self)` -> `Act::act`.
fn leading_path(span: &str) -> &str {
    let span = span.trim();
    let len = span
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b':')
        .count();
    span[..len].trim_end_matches(':')
}

/// The names a sentence of the instruction may mean by an existing public
/// item, with the display name for the notice.
fn public_names(existing: &[Decl]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for d in existing {
        match d {
            Decl::Struct {
                name, public: true, ..
            }
            | Decl::Enum {
                name, public: true, ..
            } => out.push((name.clone(), name.clone())),
            Decl::Callable {
                owner,
                name,
                public: true,
                ..
            } => {
                if let Some(o) = owner {
                    out.push((format!("{o}::{name}"), format!("{o}::{name}")));
                    out.push((o.clone(), o.clone()));
                }
                if name.len() >= 3 {
                    let shown = owner
                        .as_ref()
                        .map_or_else(|| name.clone(), |o| format!("{o}::{name}"));
                    out.push((name.clone(), shown));
                }
            }
            _ => {}
        }
    }
    out
}

/// Sentences that name an existing public item in backticks together with a
/// verb of change. A fallback for an instruction that shows no code.
fn prose_changes(instruction: &str, existing: &[Decl], already: &BTreeSet<String>) -> Vec<Change> {
    let names = public_names(existing);
    let mut out: Vec<Change> = Vec::new();
    for sentence in sentences(instruction) {
        let lower = sentence.to_ascii_lowercase();
        if KEEP_WORDS.iter().any(|k| lower.contains(k)) {
            continue;
        }
        let verb = CHANGE_WORDS.iter().any(|w| {
            lower.match_indices(w).any(|(at, _)| {
                // The verb as a word start: `change`, `changes`, `changing` is
                // not matched by a stem, so allow the common endings.
                at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphabetic()
            })
        }) || ADD_PHRASES.iter().any(|p| lower.contains(p));
        if !verb {
            continue;
        }
        for span in backtick_spans(sentence) {
            let path = leading_path(&span);
            if path.is_empty() {
                continue;
            }
            if let Some((_, shown)) = names.iter().find(|(n, _)| n == path)
                && !already.contains(shown)
                && !already.contains(path)
                && !out.iter().any(|c| &c.item == shown)
            {
                let excerpt: String = sentence.trim().chars().take(160).collect();
                out.push(Change {
                    kind: Kind::Prose,
                    item: shown.clone(),
                    new: format!("\"{excerpt}\""),
                    old: String::new(),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------- entry point

/// Notices for a contract instruction that changes a public shape defined in
/// a file the contract writes. `read` gives a file's text. A notice, never a
/// refusal: the user's instruction may ask for the change.
pub fn shape_changes(
    spec: &ContractSpec,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Vec<String> {
    let candidates = code_candidates(&spec.instruction);
    let restated: Vec<Decl> = candidates
        .iter()
        .flat_map(|c| snippet_declarations(c))
        .collect();
    let mut found: Vec<(String, Change)> = Vec::new();
    let mut flagged: BTreeSet<String> = BTreeSet::new();
    for w in &spec.writes {
        let Some(source) = read(&w.path) else {
            continue;
        };
        let rust = Language::from_path(&w.path) == Some(Language::Rust);
        let Some(existing) = rust.then(|| declarations(&source, true)).flatten() else {
            // No grammar for the file: the older token scan of unit structs.
            for name in crate::plan::unit_structs(&source) {
                if defines_with_fields(&spec.instruction, &name)
                    && !found.iter().any(|(_, c)| c.item == name)
                {
                    found.push((
                        w.path.clone(),
                        Change {
                            kind: Kind::Unit,
                            item: name,
                            new: String::new(),
                            old: String::new(),
                        },
                    ));
                }
            }
            continue;
        };
        let mut changes = compare(&existing, &restated);
        // A public unit struct given fields as text that is no item
        // (`Name { order: Order }`).
        for d in &existing {
            if let Decl::Struct {
                name,
                public: true,
                fields: Fields::Unit,
            } = d
                && defines_with_fields(&spec.instruction, name)
                && !changes.iter().any(|c| &c.item == name)
            {
                changes.push(Change {
                    kind: Kind::Unit,
                    item: name.clone(),
                    new: String::new(),
                    old: String::new(),
                });
            }
        }
        for c in &changes {
            flagged.insert(c.item.clone());
            if let Some((parent, _)) = c.item.split_once("::") {
                flagged.insert(parent.to_owned());
            }
        }
        let prose = prose_changes(&spec.instruction, &existing, &flagged);
        for c in changes.into_iter().chain(prose) {
            if !found.iter().any(|(_, f)| f.item == c.item) {
                found.push((w.path.clone(), c));
            }
        }
    }
    found
        .into_iter()
        .map(|(path, c)| match c.kind {
            Kind::Unit => format!(
                "contract {} instructs `{}` with fields, but `{path}` defines it as a unit \
                 struct that existing code may use as a value; the shape is kept unless the \
                 user's instruction asks to change it",
                spec.id, c.item
            ),
            Kind::Prose => format!(
                "contract {} asks for a change to `{}`, which `{path}` already defines as a \
                 public item that existing code may use ({}); the shape is kept unless the \
                 user's instruction asks to change it",
                spec.id, c.item, c.new
            ),
            Kind::Shape => format!(
                "contract {} instructs `{}` as `{}`, but `{path}` already defines the existing \
                 public `{}` and code that uses it may break; the shape is kept unless the \
                 user's instruction asks to change it",
                spec.id, c.item, c.new, c.old
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_state::WriteTarget;

    const SCAFFOLD: &str = "\
pub struct Thing;
pub struct Point { pub x: i32, pub y: i32 }
pub struct Pair(pub i32, pub i32);
pub struct Cache { map: u8, hits: u32 }
struct Hidden;
pub enum Shape { Circle(f64), Rect { w: f64, h: f64 }, Empty }
pub trait Act {
    fn act(&self, x: i32) -> i32;
}
pub fn run(x: i32) -> i32 { x }
fn helper(a: u8) -> u8 { a }
impl Thing {
    pub fn new() -> Self { Thing }
    fn secret(&self, n: u8) {}
}
impl Act for Thing {
    fn act(&self, x: i32) -> i32 { x }
}
";

    fn spec(instruction: &str) -> ContractSpec {
        ContractSpec {
            id: "c1".into(),
            instruction: instruction.into(),
            acceptance: "cargo test -p a".into(),
            writes: vec![WriteTarget {
                path: "a/src/lib.rs".into(),
                node: None,
            }],
            depends_on: vec![],
            slices: vec![],
            tools: vec![],
            route: None,
            attempts: 1,
            task_kind: None,
        }
    }

    fn notes(instruction: &str) -> Vec<String> {
        shape_changes(&spec(instruction), |_| Some(SCAFFOLD.into()))
    }

    fn names(notes: &[String], item: &str) -> bool {
        notes.iter().any(|n| n.contains(&format!("`{item}`")))
    }

    /// Round 7 (plan-contradicts-scaffold): the trait restated with a second
    /// parameter. Only the `Thing { .. }` phrasing was caught before.
    #[test]
    fn a_trait_restated_with_another_signature_is_named() {
        let n = notes(
            "In a/src/lib.rs, change the trait to `pub trait Act { fn act(&self, x: i32, y: i32) -> i32; }`, then update the impl to match.",
        );
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(names(&n, "Act::act"), "{n:?}");
        assert!(
            n[0].contains("existing") && n[0].contains("a/src/lib.rs"),
            "{n:?}"
        );
    }

    /// As a scripted run builds it: the scaffold of `a/src/lib.rs` and the
    /// planner's instruction. Both contradictions are named: the struct (with
    /// the older wording) and the trait method.
    #[test]
    fn the_round_7_plan_names_both_contradictions() {
        let scaffold = "/// A unit value other crates build on.\npub struct Thing;\n\n\
            /// What a Thing does to a number.\npub trait Act {\n    fn act(&self, x: i32) -> i32;\n}\n\n\
            impl Act for Thing {\n    fn act(&self, x: i32) -> i32 {\n        x + 1\n    }\n}\n";
        let instruction = "In a/src/lib.rs, redefine `pub struct Thing { pub field: i32 }` and change the trait to \
            `pub trait Act { fn act(&self, x: i32, y: i32) -> i32; }`, then update the impl to match.";
        let n = shape_changes(&spec(instruction), |_| Some(scaffold.into()));
        assert_eq!(n.len(), 2, "{n:?}");
        assert!(names(&n, "Thing") && names(&n, "Act::act"), "{n:?}");
        for text in &n {
            assert!(
                text.contains("existing") || text.contains("unit struct"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_bare_signature_or_fence_is_read_too() {
        for text in [
            "Make it `fn act(&self, x: i32, y: i32) -> i32`.",
            "Make it `fn act(&self, x: i32, y: i32) -> i32;`.",
            "New signature:\n```rust\nfn act(&self, x: i32, y: String) -> i32 { 0 }\n```\n",
            "Now we have pub fn act(&self, x: u8) -> i32; in the trait",
            "Write `impl Act for Thing { fn act(&self, x: i32, y: i32) -> i32 { x } }`",
        ] {
            let n = notes(text);
            assert!(names(&n, "Act::act") || names(&n, "act"), "{text}: {n:?}");
        }
    }

    #[test]
    fn free_fn_inherent_method_and_return_type_changes_are_named() {
        assert!(names(
            &notes("Use `pub fn run(x: i32, y: i32) -> i32`"),
            "run"
        ));
        assert!(names(
            &notes("Use `pub fn run(x: i32) -> i64 { 0 }`"),
            "run"
        ));
        assert!(names(
            &notes("Use `impl Thing { pub fn new(seed: u8) -> Self { Thing } }`"),
            "Thing::new"
        ));
        assert!(names(
            &notes("Use `pub async fn run(x: i32) -> i32`"),
            "run"
        ));
    }

    #[test]
    fn struct_shapes_are_compared_by_what_outside_code_uses() {
        // Unit gains fields: the old case, in both forms.
        assert!(names(
            &notes("redefine `pub struct Thing { pub field: i32 }`"),
            "Thing"
        ));
        let legacy = notes("Define `Thing { order: Order }` with `new(order)`");
        assert!(
            legacy[0].contains("defines it as a unit struct"),
            "{legacy:?}"
        );
        // A public field changes type, or is dropped, or one is added to an all-public struct.
        assert!(names(
            &notes("`pub struct Point { pub x: i64, pub y: i32 }`"),
            "Point"
        ));
        assert!(names(&notes("`pub struct Point { pub x: i32 }`"), "Point"));
        assert!(names(
            &notes("`pub struct Point { pub x: i32, pub y: i32, pub z: i32 }`"),
            "Point"
        ));
        assert!(names(&notes("`pub struct Pair(pub i32)`"), "Pair"));
        assert!(names(
            &notes("`pub struct Pair { pub a: i32, pub b: i32 }`"),
            "Pair"
        ));
    }

    #[test]
    fn enum_variant_shapes_are_compared() {
        assert!(names(
            &notes("`pub enum Shape { Circle(f32) }`"),
            "Shape::Circle"
        ));
        assert!(names(
            &notes("`pub enum Shape { Rect { w: f64 } }`"),
            "Shape::Rect"
        ));
        // A new variant, and a variant restated as it is, change nothing.
        assert!(notes("`pub enum Shape { Circle(f64), Triangle(f64) }`").is_empty());
    }

    #[test]
    fn additions_and_restating_the_same_shape_are_not_contradictions() {
        for text in [
            // A new item.
            "Add `pub fn area(&self) -> f64` and `pub struct Square(pub f64);`",
            "Add `pub trait Draw { fn draw(&self, canvas: &mut Canvas); }`",
            "Add `impl Thing { pub fn reset(&mut self) {} }`",
            // The same shape; names of parameters and spacing do not count.
            "Keep `pub trait Act { fn act(&self, value: i32) -> i32; }` as it is",
            "`pub fn run(n:i32)->i32`",
            "`pub struct Point { pub x: i32, pub y: i32 }`",
            "`pub struct Thing;`",
            // Prose in backticks, and code that is a use, not a definition.
            "Call `act(1)` and `Thing` then `x + y`",
            "Run `cargo test -p a` and read `Act::act`",
        ] {
            let n = notes(text);
            assert!(n.is_empty(), "{text}: {n:?}");
        }
    }

    #[test]
    fn a_private_item_may_change() {
        for text in [
            "`fn helper(a: u8, b: u8) -> u8 { a }`",
            "`struct Hidden { x: u8 }`",
            "`impl Thing { fn secret(&self, n: u8, m: u8) {} }`",
            // Fields that are private: the struct's own module is the only user.
            "`pub struct Cache { map: u16, hits: u32, extra: u8 }`",
            "Change `Hidden` and `helper`.",
        ] {
            let n = notes(text);
            assert!(n.is_empty(), "{text}: {n:?}");
        }
    }

    #[test]
    fn prose_that_changes_a_public_item_is_a_fallback() {
        let n = notes("Change `Act::act` to take a second parameter.");
        assert!(names(&n, "Act::act"), "{n:?}");
        assert!(names(&notes("Rename `run` to `execute`."), "run"));
        assert!(names(&notes("Add a field to `Thing`."), "Thing"));
        assert!(names(&notes("Remove `Shape` variants one by one"), "Shape"));
        // Kept, negated, an addition, or an item the repository does not define.
        for text in [
            "Do not change `Act::act`.",
            "Implement `Act` without changing its signature.",
            "Keep `Thing` as the repository defines it",
            "Add a method `describe` to `Act`.",
            "Change `Widget` to take a second parameter.",
            "Change the trait to take a second parameter.",
        ] {
            let n = notes(text);
            assert!(n.is_empty(), "{text}: {n:?}");
        }
    }

    #[test]
    fn a_change_is_reported_once_and_code_beats_prose() {
        let n = notes(
            "Change the trait `Act`: `pub trait Act { fn act(&self, x: i32, y: i32) -> i32; }`. Change `Act::act` to match.",
        );
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(
            n[0].contains("instructs"),
            "the structural notice wins: {n:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_rust_keeps_the_unit_struct_scan() {
        let mut s = spec("Define `Thing { x: u8 }`");
        s.writes[0].path = "a/lib.unknown".into();
        let n = shape_changes(&s, |_| Some("pub struct Thing;".into()));
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(shape_changes(&s, |_| None).is_empty());
    }

    #[test]
    fn a_file_the_contract_does_not_write_is_not_read() {
        let mut s = spec("`pub fn run(x: i32, y: i32) -> i32`");
        s.writes.clear();
        assert!(shape_changes(&s, |_| Some(SCAFFOLD.into())).is_empty());
    }
}
