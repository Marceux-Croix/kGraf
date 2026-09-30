//! The deterministic roadmap parser (decision 0064): a roadmap or plan document
//! in Markdown into presumed work, with no model call.
//!
//! It reads three structures, the ones roadmaps are written in:
//!
//! 1. **Status tables.** A table whose header has a status column (`Status`,
//!    `State`, `Progress`, `Done`) is a list of items: one per row, keyed by its
//!    first cell (a phase number, a milestone name). A heading whose text names
//!    a row's key (`### Milestone 4.5: Scheduled runs` for the row `4.5`) is that
//!    row's detail section: its code references become the row's evidence.
//! 2. **Task lists.** `- [x] done` and `- [ ] to do` items, anywhere.
//! 3. **Status headings**, only in a document with no status table (the table is
//!    then the authority, and headings such as "Milestone 1 result. Complete." are
//!    records, not items): a heading with a status word in its text, or a
//!    `Status:` line directly under it, and no items of its own beneath it.
//!
//! Headings that hold items become components, nested as the document nests
//! them. The document's status word is recorded as its **claim** ([`Claim`]),
//! never as a status: doneness is derived from the claim and the code at read
//! time (`presumed.rs`).
//!
//! **Identity.** An entry's key is a path of slugs: its enclosing headings and
//! its own key (a row's first cell, a task's text, a heading's text). The same
//! document read again yields the same keys, so a re-read updates nodes rather
//! than adding them. A renamed heading or task is a new key: the old node is
//! marked gone, the new one added. Two entries with the same key under one
//! parent are told apart by order (`~2`, `~3`), which is stable while the
//! document's order is.
//!
//! **Reused (reuse existing code).** Markdown events come from `pulldown-cmark` (MIT), the
//! parser `kgraf-ingest` already uses. Searched and not reused: crates.io for
//! roadmap, markdown task, checklist and todo parsers (`roadmap` 0.7.0,
//! `md-todo` 0.1.2, `markdown-org-extract` 0.24.1, `taskmd-core` 1.3.0): each
//! defines its own file format (a YAML DAG, one file per task, Org-mode) rather
//! than reading the status tables and headings roadmaps are written in. The
//! classification here is kGraf's own.

use crate::model::Claim;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// The parser's identity, recorded on every node it writes.
pub const PARSER: &str = "kgraf roadmap-md/1";

/// Code references kept per entry, each of paths and symbols. More are counted,
/// not kept: a detail section can name hundreds.
pub const MAX_EVIDENCE: usize = 16;

/// What one entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    /// A heading that holds items.
    Component,
    /// One unit of presumed work.
    Item,
}

/// What an entry's text names in the code, for reconciliation.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Named {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    /// References found beyond [`MAX_EVIDENCE`], not kept.
    #[serde(default)]
    pub capped: usize,
}

/// One presumed component or item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    /// The key path: enclosing component keys, then this entry's own.
    pub key: Vec<String>,
    /// The enclosing component's index in [`Roadmap::entries`], if any.
    pub parent: Option<usize>,
    pub title: String,
    /// Where it is, for a person: `Phases > row 4.5`, `Milestones > v1 > task`.
    pub section: String,
    pub claim: Claim,
    /// The status text as written (the status cell, the task marker, the
    /// heading's status phrase), cut to 200 characters.
    pub claim_text: String,
    /// The entry's own text, cut to 400 characters.
    pub excerpt: String,
    pub named: Named,
    /// `path#heading-slug` anchors that cite this entry: its own heading, or a
    /// row's detail heading. A model reading citing one of them is about this
    /// entry.
    pub anchors: Vec<String>,
}

/// What a parse found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roadmap {
    /// The document's title (its first level-1 heading), if any.
    pub title: Option<String>,
    pub entries: Vec<Entry>,
    pub status_tables: usize,
    pub table_rows: usize,
    pub tasks: usize,
    pub status_headings: usize,
}

impl Roadmap {
    pub fn items(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.kind == EntryKind::Item)
    }

    /// The structures found, for a report.
    pub fn shape(&self) -> String {
        format!(
            "{} status table(s) with {} row(s), {} task item(s), {} status heading(s)",
            self.status_tables, self.table_rows, self.tasks, self.status_headings
        )
    }
}

/// A lowercase slug: letters and digits kept, every other run one `-`.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// A `path#heading` anchor in the form every anchor here uses: the path as
/// given (a leading `./` removed), the heading slugged. A model reading's
/// `sources` are normalized with it before they are compared.
pub fn anchor(path: &str, heading: &str) -> String {
    format!("{}#{}", path.trim().trim_start_matches("./"), slug(heading))
}

/// Normalize a reading's source citation (`path` or `path#heading`) to an
/// anchor; `None` without a heading.
pub fn anchor_of_citation(citation: &str) -> Option<String> {
    let (path, heading) = citation.split_once('#')?;
    // The brief labels a chunk of a long section `path#heading [part i/n]`.
    let heading = match heading.rfind(" [part ") {
        Some(i) => &heading[..i],
        None => heading,
    }
    .trim();
    (!heading.is_empty()).then(|| anchor(path, heading))
}

fn cut(text: &str, max: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        t
    } else {
        let mut s: String = t.chars().take(max.saturating_sub(3)).collect();
        s.push_str("...");
        s
    }
}

/// The status vocabulary, per claim. Longer phrases are listed before the
/// words they contain, and matching is by whole word, so "not started" is not
/// "started" and "incomplete" is not "complete".
const VOCABULARY: &[(&str, Claim)] = &[
    ("not started", Claim::NotStarted),
    ("not yet started", Claim::NotStarted),
    ("not done", Claim::NotStarted),
    ("to do", Claim::NotStarted),
    ("todo", Claim::NotStarted),
    ("planned", Claim::NotStarted),
    ("pending", Claim::NotStarted),
    ("backlog", Claim::NotStarted),
    ("not begun", Claim::NotStarted),
    ("in progress", Claim::Partial),
    ("in-progress", Claim::Partial),
    ("underway", Claim::Partial),
    ("ongoing", Claim::Partial),
    ("partial", Claim::Partial),
    ("partially", Claim::Partial),
    ("wip", Claim::Partial),
    ("started", Claim::Partial),
    ("in flight", Claim::Partial),
    ("complete", Claim::Complete),
    ("completed", Claim::Complete),
    ("done", Claim::Complete),
    ("finished", Claim::Complete),
    ("shipped", Claim::Complete),
    ("delivered", Claim::Complete),
];

/// Emoji status marks, matched anywhere.
const MARKS: &[(&str, Claim)] = &[
    ("\u{2705}", Claim::Complete),   // white heavy check mark
    ("\u{2714}", Claim::Complete),   // heavy check mark
    ("\u{1f6a7}", Claim::Partial),   // construction sign
    ("\u{2b1c}", Claim::NotStarted), // white large square
];

/// The first status word or mark in `text`, by position, with the phrase as
/// written. `None` when it names no status.
fn claim_of(text: &str) -> Option<(Claim, String)> {
    let lower = text.to_lowercase();
    let mut best: Option<(usize, usize, Claim)> = None;
    let mut consider = |at: usize, len: usize, claim: Claim| {
        let better = match best {
            None => true,
            Some((b_at, b_len, _)) => at < b_at || (at == b_at && len > b_len),
        };
        if better {
            best = Some((at, len, claim));
        }
    };
    for (word, claim) in VOCABULARY {
        let mut from = 0;
        while let Some(i) = lower[from..].find(word) {
            let at = from + i;
            let end = at + word.len();
            let before = lower[..at].chars().next_back();
            let after = lower[end..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '-');
            if boundary(before) && boundary(after) {
                consider(at, word.len(), *claim);
                break;
            }
            from = end;
        }
    }
    for (mark, claim) in MARKS {
        if let Some(at) = lower.find(mark) {
            consider(at, mark.len(), *claim);
        }
    }
    // `lower` and `text` can differ in length for a few scripts; the phrase is
    // taken from `lower`, which is what matched.
    best.map(|(at, len, claim)| (claim, lower[at..at + len].to_owned()))
}

/// Extensions whose presence makes a code span a path.
const PATH_EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "ts", "tsx", "js", "jsx", "mjs", "md", "toml", "json", "yaml", "yml", "go",
    "java", "c", "h", "cc", "cpp", "hpp", "rb", "cs", "php", "sh", "sql", "html", "css", "lock",
    "txt",
];

/// What a code span names: a path, a symbol, or nothing kGraf can check.
fn classify(code: &str) -> Option<(bool, String)> {
    let c = code.trim();
    if c.is_empty()
        || c.len() > 200
        || c.contains(char::is_whitespace)
        || c.contains("://")
        || c.starts_with('-')
        || c.starts_with('/')
        || c.starts_with('~')
        || c.starts_with('$')
    {
        return None;
    }
    // A commit id is not a path or a symbol.
    if (7..=40).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let c = c.trim_start_matches("./");
    let ext = c.rsplit_once('.').map(|(_, e)| e);
    let looks_like_path = c.contains('/') || ext.is_some_and(|e| PATH_EXTENSIONS.contains(&e));
    if looks_like_path {
        // `src/lib.rs:42` and `src/lib.rs:42-50` name the file.
        let path = match c.split_once(':') {
            Some((p, rest)) if rest.chars().all(|ch| ch.is_ascii_digit() || ch == '-') => p,
            _ => c,
        };
        let ok = path
            .chars()
            .all(|ch| ch.is_alphanumeric() || "/._-+@".contains(ch))
            && !path.split('/').any(|seg| seg == "..");
        return ok.then(|| (true, path.trim_end_matches('/').to_owned()));
    }
    let bare = c.trim_end_matches("()");
    let name = bare.rsplit("::").next().unwrap_or(bare);
    let ident = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
        && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
        && bare
            .split("::")
            .all(|seg| !seg.is_empty() && seg.chars().all(|ch| ch.is_alphanumeric() || ch == '_'));
    if !ident {
        return None;
    }
    let uppers = name.chars().filter(|ch| ch.is_uppercase()).count();
    let lowers = name.chars().filter(|ch| ch.is_lowercase()).count();
    let camel = name.starts_with(|ch: char| ch.is_uppercase()) && uppers >= 2 && lowers >= 1;
    let symbolish = name.contains('_') || bare.contains("::") || c.ends_with("()") || camel;
    symbolish.then(|| (false, name.to_owned()))
}

fn named_from(codes: &[String]) -> Named {
    let mut n = Named::default();
    for code in codes {
        let Some((is_path, value)) = classify(code) else {
            continue;
        };
        let list = if is_path {
            &mut n.paths
        } else {
            &mut n.symbols
        };
        if list.contains(&value) {
            continue;
        }
        if list.len() >= MAX_EVIDENCE {
            n.capped += 1;
        } else {
            list.push(value);
        }
    }
    n
}

/// One heading and the prose beneath it (up to the next heading).
#[derive(Default, Debug)]
struct Heading {
    level: usize,
    text: String,
    /// Index of the enclosing heading (level 2 and deeper; a level-1 heading is
    /// the title and encloses nothing).
    parent: Option<usize>,
    body: String,
    codes: Vec<String>,
}

#[derive(Debug)]
struct Row {
    heading: Option<usize>,
    header: Vec<String>,
    cells: Vec<(String, Vec<String>)>,
}

#[derive(Debug)]
struct Task {
    heading: Option<usize>,
    checked: bool,
    text: String,
    codes: Vec<String>,
}

/// Body text kept per heading for claim and excerpt purposes.
const BODY_KEEP: usize = 4_000;

/// Parse one document. `path` is its repository-relative path, used in anchors.
pub fn parse(path: &str, text: &str) -> Roadmap {
    let mut headings: Vec<Heading> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut tasks: Vec<Task> = Vec::new();
    let mut title: Option<String> = None;

    // Walk state.
    let mut current: Option<usize> = None;
    let mut in_heading: Option<Heading> = None;
    let mut table_header: Option<Vec<String>> = None;
    let mut row: Option<Vec<(String, Vec<String>)>> = None;
    let mut cell: Option<(String, Vec<String>)> = None;
    // Items open, innermost last: (task marker seen, text, codes, nested list seen).
    let mut items: Vec<(Option<bool>, String, Vec<String>, bool)> = Vec::new();

    let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = Some(Heading {
                    level: match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    },
                    ..Heading::default()
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(mut h) = in_heading.take() {
                    h.text = cut(&h.text, 300);
                    if h.level == 1 {
                        if title.is_none() {
                            title = Some(h.text.clone());
                        }
                        current = None;
                        continue;
                    }
                    let mut p = current;
                    while let Some(i) = p {
                        if headings[i].level < h.level {
                            break;
                        }
                        p = headings[i].parent;
                    }
                    h.parent = p;
                    headings.push(h);
                    current = Some(headings.len() - 1);
                }
            }
            Event::Start(Tag::Table(_)) => table_header = Some(Vec::new()),
            Event::End(TagEnd::Table) => table_header = None,
            Event::Start(Tag::TableHead) => {
                row = Some(Vec::new());
            }
            Event::End(TagEnd::TableHead) => {
                if let (Some(h), Some(r)) = (table_header.as_mut(), row.take()) {
                    *h = r.into_iter().map(|(t, _)| cut(&t, 80)).collect();
                }
            }
            Event::Start(Tag::TableRow) => row = Some(Vec::new()),
            Event::End(TagEnd::TableRow) => {
                if let (Some(h), Some(r)) = (table_header.as_ref(), row.take()) {
                    rows.push(Row {
                        heading: current,
                        header: h.clone(),
                        cells: r,
                    });
                }
            }
            Event::Start(Tag::TableCell) => cell = Some((String::new(), Vec::new())),
            Event::End(TagEnd::TableCell) => {
                if let (Some(r), Some(c)) = (row.as_mut(), cell.take()) {
                    r.push(c);
                }
            }
            Event::Start(Tag::Item) => items.push((None, String::new(), Vec::new(), false)),
            Event::End(TagEnd::Item) => {
                if let Some((Some(checked), text, codes, _)) = items.pop() {
                    tasks.push(Task {
                        heading: current,
                        checked,
                        text: cut(&text, 400),
                        codes,
                    });
                }
            }
            Event::Start(Tag::List(_)) => {
                if let Some(top) = items.last_mut() {
                    top.3 = true;
                }
            }
            Event::TaskListMarker(checked) => {
                if let Some(top) = items.last_mut() {
                    top.0 = Some(checked);
                }
            }
            Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) | Event::Html(t) => {
                let mut open = Open {
                    heading: &mut in_heading,
                    cell: &mut cell,
                    items: &mut items,
                };
                open.push(&t, &mut headings, current);
            }
            Event::SoftBreak | Event::HardBreak => {
                let mut open = Open {
                    heading: &mut in_heading,
                    cell: &mut cell,
                    items: &mut items,
                };
                open.push(" ", &mut headings, current);
            }
            _ => {}
        }
    }
    // Code spans are recorded separately; a second pass collects them with the
    // same routing, since `Event::Code` above only added their text.
    collect_codes(text, &mut headings, &mut rows, &mut tasks);
    build(path, title, headings, rows, tasks)
}

/// What text goes to while walking: the heading being read, the table cell
/// being read, the open list items.
struct Open<'a> {
    heading: &'a mut Option<Heading>,
    cell: &'a mut Option<(String, Vec<String>)>,
    items: &'a mut [(Option<bool>, String, Vec<String>, bool)],
}

impl Open<'_> {
    /// Route text to whatever is open: a heading's text, a table cell, the
    /// innermost list item (until a nested list starts in it), and always the
    /// current heading's body.
    fn push(&mut self, t: &str, headings: &mut [Heading], current: Option<usize>) {
        if let Some(h) = self.heading.as_mut() {
            h.text.push_str(t);
            return;
        }
        if let Some((text, _)) = self.cell.as_mut() {
            text.push_str(t);
        }
        if let Some(top) = self.items.last_mut()
            && !top.3
        {
            top.1.push_str(t);
        }
        if let Some(i) = current
            && headings[i].body.len() < BODY_KEEP
        {
            headings[i].body.push_str(t);
        }
    }
}

/// The code spans of each heading body, table cell and task, in a second walk
/// with the same structure: kept apart from the text walk so each code span is
/// routed exactly once.
fn collect_codes(text: &str, headings: &mut [Heading], rows: &mut [Row], tasks: &mut [Task]) {
    let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut heading_index: Option<usize> = None;
    let mut seen_headings = 0usize;
    let mut in_heading = false;
    let mut heading_level = 0;
    let mut row_index = 0usize;
    let mut in_body_row = false;
    let mut cell_index = 0usize;
    let mut in_cell = false;
    let mut task_index = 0usize;
    // (has a task marker, nested list seen, codes)
    let mut items: Vec<(bool, bool, Vec<String>)> = Vec::new();
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                heading_level = level as usize;
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                if heading_level == 1 {
                    heading_index = None;
                } else {
                    heading_index = Some(seen_headings);
                    seen_headings += 1;
                }
            }
            Event::Start(Tag::TableRow) => {
                in_body_row = true;
                cell_index = 0;
            }
            Event::End(TagEnd::TableRow) => {
                if in_body_row {
                    row_index += 1;
                }
                in_body_row = false;
            }
            Event::Start(Tag::TableCell) => in_cell = true,
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                cell_index += 1;
            }
            Event::Start(Tag::Item) => items.push((false, false, Vec::new())),
            Event::End(TagEnd::Item) => {
                if let Some((true, _, codes)) = items.pop() {
                    if let Some(t) = tasks.get_mut(task_index) {
                        t.codes = codes;
                    }
                    task_index += 1;
                }
            }
            Event::Start(Tag::List(_)) => {
                if let Some(top) = items.last_mut() {
                    top.1 = true;
                }
            }
            Event::TaskListMarker(_) => {
                if let Some(top) = items.last_mut() {
                    top.0 = true;
                }
            }
            Event::Code(c) => {
                if in_heading {
                    continue;
                }
                let c = c.to_string();
                if in_cell
                    && in_body_row
                    && let Some(cell) = rows
                        .get_mut(row_index)
                        .and_then(|r| r.cells.get_mut(cell_index))
                {
                    cell.1.push(c.clone());
                }
                if let Some(top) = items.last_mut()
                    && !top.1
                {
                    top.2.push(c.clone());
                }
                if let Some(i) = heading_index
                    && let Some(h) = headings.get_mut(i)
                {
                    h.codes.push(c);
                }
            }
            _ => {}
        }
    }
}

/// The index of a status column in a header, and of the key column.
fn table_columns(header: &[String]) -> Option<(usize, usize)> {
    let is_status = |h: &str| {
        let h = h.trim().to_lowercase();
        matches!(
            h.as_str(),
            "status" | "state" | "progress" | "done" | "complete" | "completed" | "completion"
        ) || h.starts_with("status")
    };
    let status = header.iter().position(|h| is_status(h))?;
    let key = if status == 0 { 1 } else { 0 };
    (key < header.len()).then_some((status, key))
}

/// Whether heading `text` is the detail section of the row keyed `key`: after
/// an optional label word (`Phase`, `Milestone`, `Stage`, `Step`, `Part`,
/// `Version`, `Release`), the heading starts with the key and then ends or
/// continues with `:` (`Milestone 4.5: Scheduled runs`). "Milestone 1 result" and
/// "Milestone 1.5" are not row `1`'s.
/// A column whose values answer "is it done?" rather than naming a status.
fn is_done_column(header: &str) -> bool {
    matches!(
        header.trim().to_lowercase().as_str(),
        "done" | "complete" | "completed" | "completion"
    )
}

/// A done-type column's value: yes, y, x, true or a check mark is
/// done; no, n, false, a dash or an empty cell is not started. Anything else
/// falls back to the status vocabulary.
fn done_value(cell: &str) -> Option<Claim> {
    match cell.trim().to_lowercase().as_str() {
        "yes" | "y" | "x" | "[x]" | "true" | "\u{2713}" | "\u{2714}" | "\u{2705}" => {
            Some(Claim::Complete)
        }
        "no" | "n" | "false" | "-" | "\u{2014}" | "[ ]" | "" => Some(Claim::NotStarted),
        _ => None,
    }
}

#[cfg(test)]
fn names_row(text: &str, key: &str) -> bool {
    let key = key.trim().to_lowercase();
    !key.is_empty() && row_keys_of(text).contains(&key)
}

/// The row keys heading `text` can be the detail section of, lowercased: the
/// text after an optional label word, whole and up to each `:` in it. Computed
/// once per heading, so matching a row is one map lookup (matching
/// every row against every heading took 202 s for 20,000 of each).
fn row_keys_of(text: &str) -> Vec<String> {
    let t = text.trim().to_lowercase();
    let labels = [
        "phase ",
        "milestone ",
        "stage ",
        "step ",
        "part ",
        "version ",
        "release ",
    ];
    let rest = labels.iter().find_map(|l| t.strip_prefix(l)).unwrap_or(&t);
    let mut keys = vec![rest.to_owned()];
    for (i, _) in rest.match_indices(':') {
        keys.push(rest[..i].to_owned());
    }
    keys.retain(|k| !k.is_empty());
    keys
}

fn build(
    path: &str,
    title: Option<String>,
    headings: Vec<Heading>,
    rows: Vec<Row>,
    tasks: Vec<Task>,
) -> Roadmap {
    let mut out = Roadmap {
        title,
        ..Roadmap::default()
    };

    // Items, each with the heading it sits under.
    struct Pending {
        heading: Option<usize>,
        own_key: String,
        title: String,
        section_tail: String,
        claim: Claim,
        claim_text: String,
        excerpt: String,
        codes: Vec<String>,
        anchors: Vec<String>,
    }
    let mut pending: Vec<Pending> = Vec::new();

    // Row key -> the first heading that is its detail section.
    let mut detail_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, h) in headings.iter().enumerate() {
        for k in row_keys_of(&h.text) {
            detail_of.entry(k).or_insert(i);
        }
    }

    let mut tables_seen = std::collections::BTreeSet::new();
    for r in &rows {
        let Some((status_col, key_col)) = table_columns(&r.header) else {
            continue;
        };
        tables_seen.insert((r.heading, r.header.clone()));
        let cell_text = |i: usize| r.cells.get(i).map(|c| c.0.trim()).unwrap_or("");
        let key = cell_text(key_col);
        if key.is_empty() {
            continue;
        }
        out.table_rows += 1;
        let status = cell_text(status_col);
        let done_column = r.header.get(status_col).is_some_and(|h| is_done_column(h));
        let claim = done_column
            .then(|| done_value(status))
            .flatten()
            .or_else(|| claim_of(status).map(|c| c.0))
            .unwrap_or(Claim::Unstated);
        let description = (0..r.cells.len())
            .find(|&i| i != status_col && i != key_col && !cell_text(i).is_empty())
            .map(cell_text)
            .unwrap_or("");
        let key_header = r.header.get(key_col).map(|h| h.trim()).unwrap_or("");
        let numbered = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == 'v' || c == 'V');
        let head = if numbered && !key_header.is_empty() {
            format!("{key_header} {key}")
        } else {
            key.to_owned()
        };
        let title = if description.is_empty() {
            head
        } else {
            format!("{head}: {description}")
        };
        let mut codes: Vec<String> = r.cells.iter().flat_map(|c| c.1.clone()).collect();
        let mut anchors = Vec::new();
        if let Some(h) = detail_of.get(&key.to_lowercase()).map(|&i| &headings[i]) {
            codes.extend(h.codes.iter().cloned());
            anchors.push(anchor(path, &h.text));
        }
        pending.push(Pending {
            heading: r.heading,
            own_key: slug(key),
            title: cut(&title, 160),
            section_tail: format!("row {key}"),
            claim,
            claim_text: cut(status, 200),
            excerpt: cut(description, 400),
            codes,
            anchors,
        });
    }
    out.status_tables = tables_seen.len();

    for t in &tasks {
        out.tasks += 1;
        let claim = if t.checked {
            Claim::Complete
        } else {
            match claim_of(&t.text) {
                Some((Claim::Partial, _)) => Claim::Partial,
                _ => Claim::NotStarted,
            }
        };
        let key: String = slug(&t.text).chars().take(48).collect();
        pending.push(Pending {
            heading: t.heading,
            own_key: key.trim_end_matches('-').to_owned(),
            title: cut(&t.text, 160),
            section_tail: "task".into(),
            claim,
            claim_text: if t.checked { "[x]" } else { "[ ]" }.to_owned(),
            excerpt: cut(&t.text, 400),
            codes: t.codes.clone(),
            anchors: Vec::new(),
        });
    }

    // Headings with items beneath them.
    let mut holds = vec![false; headings.len()];
    for p in &pending {
        let mut h = p.heading;
        while let Some(i) = h {
            holds[i] = true;
            h = headings[i].parent;
        }
    }

    if out.status_tables == 0 {
        for (i, h) in headings.iter().enumerate() {
            if holds[i] {
                continue;
            }
            let from_text = claim_of(&h.text);
            let from_status_line = || {
                let body = h.body.trim_start();
                let lower = body.to_lowercase();
                lower
                    .strip_prefix("status:")
                    .map(|_| &body["status:".len()..])
                    .and_then(|rest| claim_of(&rest.chars().take(200).collect::<String>()))
            };
            let Some((claim, phrase)) = from_text.or_else(from_status_line) else {
                continue;
            };
            out.status_headings += 1;
            pending.push(Pending {
                heading: h.parent,
                own_key: slug(&h.text),
                title: cut(&h.text, 160),
                section_tail: "heading".into(),
                claim,
                claim_text: phrase,
                excerpt: cut(&h.body, 400),
                codes: h.codes.clone(),
                anchors: vec![anchor(path, &h.text)],
            });
            let mut p = h.parent;
            while let Some(j) = p {
                holds[j] = true;
                p = headings[j].parent;
            }
        }
    }

    // Components: headings holding items, in document order, keyed by their
    // slug path. `entry_of[heading]` is the component's entry index.
    let mut entry_of: Vec<Option<usize>> = vec![None; headings.len()];
    // Keyed by kind too: a component's id and an item's differ by
    // their `component/` and `item/` prefix, so a new heading named like an
    // existing task must not push that task to `~2`.
    let mut taken: std::collections::BTreeMap<(Option<usize>, EntryKind, String), usize> =
        std::collections::BTreeMap::new();
    let mut unique = |parent: Option<usize>, kind: EntryKind, key: String| -> String {
        let n = taken.entry((parent, kind, key.clone())).or_insert(0);
        *n += 1;
        if *n == 1 { key } else { format!("{key}~{n}") }
    };
    let names = |i: usize| -> Vec<String> {
        let mut chain = Vec::new();
        let mut h = Some(i);
        while let Some(j) = h {
            chain.push(headings[j].text.clone());
            h = headings[j].parent;
        }
        chain.reverse();
        chain
    };
    for (i, h) in headings.iter().enumerate() {
        if !holds[i] {
            continue;
        }
        let parent = h.parent.and_then(|p| entry_of[p]);
        let own = unique(parent, EntryKind::Component, slug(&h.text));
        let mut key = parent
            .map(|p| out.entries[p].key.clone())
            .unwrap_or_default();
        key.push(own);
        out.entries.push(Entry {
            kind: EntryKind::Component,
            key,
            parent,
            title: h.text.clone(),
            section: names(i).join(" > "),
            claim: claim_of(&h.text).map_or(Claim::Unstated, |c| c.0),
            claim_text: String::new(),
            excerpt: cut(&h.body, 400),
            named: Named::default(),
            anchors: vec![anchor(path, &h.text)],
        });
        entry_of[i] = Some(out.entries.len() - 1);
    }
    for p in pending {
        let parent = p.heading.and_then(|h| entry_of[h]);
        let own = unique(parent, EntryKind::Item, p.own_key);
        let mut key = parent
            .map(|i| out.entries[i].key.clone())
            .unwrap_or_default();
        key.push(own);
        let section = match p.heading {
            Some(h) => format!("{} > {}", names(h).join(" > "), p.section_tail),
            None => p.section_tail,
        };
        out.entries.push(Entry {
            kind: EntryKind::Item,
            key,
            parent,
            title: p.title,
            section,
            claim: p.claim,
            claim_text: p.claim_text,
            excerpt: p.excerpt,
            named: named_from(&p.codes),
            anchors: p.anchors,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_words_are_whole_words_and_the_first_one_wins() {
        let c = |t: &str| claim_of(t).map(|c| c.0);
        assert_eq!(c("Complete (negative result)"), Some(Claim::Complete));
        assert_eq!(
            c("PARTIAL (a complete rewrite pending)"),
            Some(Claim::Partial)
        );
        assert_eq!(c("IN PROGRESS"), Some(Claim::Partial));
        assert_eq!(c("Not started (after launch)"), Some(Claim::NotStarted));
        assert_eq!(c("incomplete"), None);
        assert_eq!(c("Doneness is derived"), None);
        assert_eq!(c("\u{2705} shipped"), Some(Claim::Complete));
        assert_eq!(c("Scheduled runs"), None);
    }

    #[test]
    fn code_spans_are_classified_as_paths_symbols_or_nothing() {
        let k = |c: &str| classify(c);
        assert_eq!(k("src/lib.rs:42"), Some((true, "src/lib.rs".into())));
        assert_eq!(k("Cargo.toml"), Some((true, "Cargo.toml".into())));
        assert_eq!(k("export_csv()"), Some((false, "export_csv".into())));
        assert_eq!(
            k("kgraf_graph::GraphStore"),
            Some((false, "GraphStore".into()))
        );
        assert_eq!(k("ApprovedPlan"), Some((false, "ApprovedPlan".into())));
        assert_eq!(k("6050c63"), None, "a commit id");
        assert_eq!(k("kgraf index"), None);
        assert_eq!(k("--corpus"), None);
        assert_eq!(k("kgraf"), None, "a plain word is not checkable");
        assert_eq!(k("../escape.rs"), None);
        assert_eq!(k("https://example.com/a.rs"), None);
    }

    #[test]
    fn a_status_table_is_the_authority_and_links_detail_sections() {
        let doc = "# Roadmap\n\n## Phases\n\n| Milestone | Description | Status |\n|---|---|---|\n\
                   | 1 | Parser | Complete |\n| 1.5 | Export | PARTIAL (`export_csv` only) |\n\
                   | 2 | Import | Not started |\n\n## Detail\n\n### Milestone 1: Parser\n\nIn `src/parse.rs`.\n\n\
                   ### Milestone 1 result (2026-01-01). Complete.\n\nNoise.\n\n### Milestone 1.5: Export\n\nSee `src/export.rs`.\n";
        let r = parse("ROADMAP.md", doc);
        assert_eq!(r.title.as_deref(), Some("Roadmap"));
        assert_eq!(
            (r.status_tables, r.table_rows, r.status_headings),
            (1, 3, 0)
        );
        let items: Vec<&Entry> = r.items().collect();
        assert_eq!(items.len(), 3, "headings are not items beside a table");
        let keys: Vec<String> = items.iter().map(|e| e.key.join("/")).collect();
        assert_eq!(keys, ["phases/1", "phases/1-5", "phases/2"]);
        assert_eq!(items[0].title, "Milestone 1: Parser");
        assert_eq!(
            items.iter().map(|e| e.claim).collect::<Vec<_>>(),
            [Claim::Complete, Claim::Partial, Claim::NotStarted]
        );
        assert_eq!(
            items[0].named.paths,
            ["src/parse.rs"],
            "from its detail section"
        );
        assert_eq!(items[0].anchors, ["ROADMAP.md#milestone-1-parser"]);
        assert_eq!(items[1].named.symbols, ["export_csv"]);
        assert_eq!(items[1].named.paths, ["src/export.rs"]);
        assert!(items[2].named.paths.is_empty());
        assert_eq!(items[1].section, "Phases > row 1.5");
        let components: Vec<&Entry> = r
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Component)
            .collect();
        assert_eq!(components.len(), 1, "only headings that hold items");
    }

    #[test]
    fn task_lists_and_status_headings_nest_under_their_headings() {
        let doc = "# Plan\n\n## Milestone A\n\n- [x] Write `src/a.rs`\n- [ ] Test it\n  - [ ] nested sub-task\n\n\
                   ## Milestone B (in progress)\n\nStatus: partial.\n\n## Milestone C\n\nStatus: not started\n\n## Notes\n\nNothing.\n";
        let r = parse("PLAN.md", doc);
        let items: Vec<(String, Claim)> = r.items().map(|e| (e.key.join("/"), e.claim)).collect();
        assert_eq!(
            items,
            [
                ("milestone-a/write-src-a-rs".to_owned(), Claim::Complete),
                ("milestone-a/nested-sub-task".to_owned(), Claim::NotStarted),
                ("milestone-a/test-it".to_owned(), Claim::NotStarted),
                ("milestone-b-in-progress".to_owned(), Claim::Partial),
                ("milestone-c".to_owned(), Claim::NotStarted),
            ]
        );
        let a = r.items().next().expect("a");
        assert_eq!(a.named.paths, ["src/a.rs"]);
        assert_eq!(r.status_headings, 2);
    }

    #[test]
    fn the_same_key_twice_under_one_parent_is_told_apart_by_order() {
        let doc = "## Todo\n\n- [ ] Same\n- [x] Same\n";
        let r = parse("TODO.md", doc);
        let keys: Vec<String> = r.items().map(|e| e.key.join("/")).collect();
        assert_eq!(keys, ["todo/same", "todo/same~2"]);
    }

    /// A heading named like an existing task, added under the same
    /// parent, left the task's key alone; it was pushed to `~2` before.
    #[test]
    fn a_new_heading_named_like_a_task_does_not_move_the_task() {
        let before = parse("T.md", "## A\n\n- [ ] Setup\n");
        let after = parse(
            "T.md",
            "## A\n\n- [ ] Setup\n\n### Setup\n\n- [ ] Install\n",
        );
        let key_of = |r: &Roadmap, title: &str, kind: EntryKind| {
            r.entries
                .iter()
                .find(|e| e.title == title && e.kind == kind)
                .map(|e| e.key.join("/"))
                .expect(title)
        };
        assert_eq!(key_of(&before, "Setup", EntryKind::Item), "a/setup");
        assert_eq!(key_of(&after, "Setup", EntryKind::Item), "a/setup");
        assert_eq!(key_of(&after, "Setup", EntryKind::Component), "a/setup");
    }

    /// A done-type column's yes, no and x are read.
    #[test]
    fn done_columns_read_yes_no_and_x() {
        let doc = "| Task | Done |\n|---|---|\n| a | yes |\n| b | no |\n| c | x |\n| d | |\n\
                   | e | in progress |\n";
        let claims: Vec<Claim> = parse("T.md", doc).items().map(|e| e.claim).collect();
        assert_eq!(
            claims,
            [
                Claim::Complete,
                Claim::NotStarted,
                Claim::Complete,
                Claim::NotStarted,
                Claim::Partial
            ]
        );
    }

    /// Rows are matched to their detail headings by a map, not by
    /// a scan of every heading per row. 20,000 rows and 20,000 headings took
    /// 202 s in a debug build before, so a quadratic regression shows as a test that
    /// does not finish. The elapsed time is checked only when `KGRAF_TIME_BUDGET`
    /// is set, since it depends on machine load.
    #[test]
    fn many_rows_and_headings_parse_and_link_every_row() {
        let n = 20_000;
        let mut doc = String::from("## P\n\n| Milestone | What | Status |\n|---|---|---|\n");
        for i in 0..n {
            doc.push_str(&format!("| {i} | thing {i} | Complete |\n"));
        }
        for i in 0..n {
            doc.push_str(&format!("\n### Phase {i}: thing\n\nIn `src/m{i}.rs`.\n"));
        }
        let started = std::time::Instant::now();
        let r = parse("R.md", &doc);
        let took = started.elapsed();
        assert_eq!(r.table_rows, n);
        let last = r.items().last().expect("last");
        assert_eq!(last.named.paths, [format!("src/m{}.rs", n - 1)]);
        eprintln!("parsed {n} rows and headings in {took:?}");
        if std::env::var_os("KGRAF_TIME_BUDGET").is_some_and(|v| !v.is_empty() && v != "0") {
            assert!(took < std::time::Duration::from_secs(30), "{took:?}");
        }
    }

    #[test]
    fn a_row_links_only_the_heading_that_names_it() {
        assert!(names_row("Milestone 4.5: Scheduled runs", "4.5"));
        assert!(names_row("Milestone 1: Code graph", "1"));
        assert!(!names_row(
            "Milestone 1 result (2026-09-25). Complete.",
            "1"
        ));
        assert!(!names_row("Milestone 1.5: Partition", "1"));
        assert!(names_row("Beta", "Beta"));
    }
}
