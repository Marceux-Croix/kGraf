//! `kgraf plan`: presumed work nodes from the project's roadmap documents
//! (decision 0064), and the report of what they stand for now.
//!
//! - `kgraf plan --from-roadmap [--source FILE]...` reads the roadmap documents
//!   (discovered, or the ones named) into presumed work nodes. No model call.
//!   With no roadmap found and none named it fails, naming what it looked for.
//! - `kgraf plan` lists every presumed node with its derived state.
//! - `kgraf index` runs the same read after indexing, by default
//!   ([`presume_after_index`]); `--no-presume` skips it, and says so.
//!
//! The model path is `kgraf orchestrate --corpus`: its reading's work items are
//! presumed too (`orchestrate_cmd.rs`), through the ledgered interpretation call.

use anyhow::Result;
use kgraf_graph::{GraphStore, WorkKind};
use kgraf_interpret::presumed::{self, PresumedState, PresumedView};
use kgraf_interpret::{InterpretError, SourceReport};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The sources to read: the named ones, else the discovered ones. None of
/// either is [`InterpretError::NoRoadmap`].
pub fn sources(store: &GraphStore, named: &[String]) -> Result<Vec<String>, InterpretError> {
    if !named.is_empty() {
        return Ok(named.to_vec());
    }
    let found = presumed::discover(store.root());
    if found.is_empty() {
        return Err(InterpretError::NoRoadmap {
            looked: presumed::looked_for(),
        });
    }
    Ok(found)
}

pub fn print_source(r: &SourceReport) {
    println!("roadmap    {}: {}", r.source, r.shape);
    let s = &r.sync;
    println!(
        "  presumed {} item(s), {} component(s): {} new, {} updated, {} unchanged, {} returned, \
         {} kept (changed by someone else), {} kept (adopted by real work), {} gone; \
         {} status claim(s) changed",
        r.items,
        r.components,
        s.inserted.len(),
        s.updated.len(),
        s.unchanged,
        s.returned.len(),
        s.kept_edited.len(),
        s.kept_adopted.len(),
        s.gone.len(),
        s.claims_updated.len()
    );
    if r.items == 0 {
        println!(
            "  no items: no status table, task list or status heading was found in {}",
            r.source
        );
    }
}

/// Counts of the derived states of the items presumed from `source`.
pub fn print_states(views: &[PresumedView], source: &str) {
    let items: Vec<&PresumedView> = views
        .iter()
        .filter(|v| v.source == source && v.kind == WorkKind::Item && !v.gone)
        .collect();
    let count = |s: PresumedState| {
        items
            .iter()
            .filter(|v| v.adopted_by.is_none() && v.state == s)
            .count()
    };
    let adopted = items.iter().filter(|v| v.adopted_by.is_some()).count();
    let disputed = items.iter().filter(|v| v.detail.is_some()).count();
    println!(
        "  now      {} presumed done, {} partly done, {} remaining, {} adopted by real work; \
         {} where the document and the code disagree (`kgraf plan` lists them)",
        count(PresumedState::Done),
        count(PresumedState::Partial),
        count(PresumedState::Remaining),
        adopted,
        disputed
    );
}

/// Print what [`presumed::retire_missing`] retired.
fn print_retired(retired: &[(String, kgraf_graph::PresumedSync)]) {
    for (source, sync) in retired {
        println!(
            "roadmap    {source}: no longer there (moved or deleted); {} of its node(s) marked \
             gone, adopted ones kept",
            sync.gone.len()
        );
    }
}

/// `kgraf plan --from-roadmap`. A source that fails is an error, after the
/// others were read: this command exists to read them, so it fails loud.
pub fn from_roadmap(store: &GraphStore, named: &[String]) -> Result<()> {
    let now = now_ms();
    // Retired first, whatever discovery finds: a roadmap moved to a name
    // discovery does not know is still gone from where it was.
    print_retired(&presumed::retire_missing(store, now)?);
    let sources = sources(store, named)?;
    // The shared read loop: the same one `presume_spinup` uses, so
    // this command and `kgraf index`'s and the shell's spin-up reads cannot
    // drift apart on how a failing source is handled.
    let (reports, failed) = presumed::read_sources(store, &sources, now);
    let views = presumed::views(store)?;
    for r in &reports {
        print_source(r);
        print_states(&views, &r.source);
    }
    for (source, e) in &failed {
        println!("roadmap    {source}: not read: {e}");
    }
    if !failed.is_empty() {
        let each: Vec<String> = failed.iter().map(|(s, e)| format!("{s}: {e}")).collect();
        anyhow::bail!(
            "{} roadmap document(s) not read: {}",
            failed.len(),
            each.join("; ")
        );
    }
    Ok(())
}

/// After `kgraf index`, and before a corpus reading: the same read, reported,
/// and **never a failure**. The caller's own work is already done
/// (`done` says what, for example "the index was committed"), so a document
/// that cannot be read, or a graph error while presuming, is a stated line. No
/// roadmap is said too: `kgraf plan --from-roadmap --source` reads a document
/// discovery does not find.
pub fn presume_after_index(store: &GraphStore, done: &str) {
    if let Err(e) = presume_after_index_inner(store, done) {
        println!("presumed   error: {e:#} ({done}; presumed work nodes were not updated)");
    }
}

/// Reads through [`presumed::presume_spinup`] (decision 0064's spin-up read):
/// the same function the desktop shell calls when it opens a project, so the
/// discovery, the read-each-source loop and the retirement are decided once.
fn presume_after_index_inner(store: &GraphStore, done: &str) -> Result<()> {
    let now = now_ms();
    let report = presumed::presume_spinup(store, now)?;
    print_retired(&report.retired);
    if let Some(looked) = &report.no_roadmap {
        println!(
            "presumed   none: no roadmap found (looked for {looked}); \
             `kgraf plan --from-roadmap --source FILE` reads another document"
        );
        return Ok(());
    }
    let views = presumed::views(store)?;
    for r in &report.sources {
        print_source(r);
        print_states(&views, &r.source);
    }
    for (source, e) in &report.failed {
        println!("presumed   error: {source} not read: {e} ({done})");
    }
    Ok(())
}

/// `kgraf plan`: every presumed item, grouped by source, with its state and
/// why. A node its source no longer lists prints as gone, and one real work
/// adopted as adopted.
pub fn list(store: &GraphStore) -> Result<()> {
    let views = presumed::views(store)?;
    if views.is_empty() {
        println!(
            "no presumed work nodes: `kgraf plan --from-roadmap` reads the roadmap, and \
             `kgraf orchestrate --corpus` presumes a model's reading"
        );
        return Ok(());
    }
    let mut source = String::new();
    for v in views.iter().filter(|v| v.kind == WorkKind::Item) {
        if v.source != source {
            source = v.source.clone();
            println!("{} ({})", v.source, v.parser);
        }
        let state = match (&v.adopted_by, v.gone) {
            (Some(by), _) => format!("adopted by {by}"),
            (None, true) => "gone from its source".to_owned(),
            (None, false) => v.state.word().to_owned(),
        };
        let mut line = format!("  {state:<22} {}", v.title);
        if !v.claim_text.is_empty() {
            line.push_str(&format!(" [says: {}]", cut(&v.claim_text, 40)));
        }
        if v.edited {
            line.push_str(" [changed by someone else; re-reads leave it]");
        }
        println!("{line}");
        if let Some(d) = &v.detail {
            println!("  {:<22} {}", "", cut(d, 160));
        }
    }
    Ok(())
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut s: String = text.chars().take(max - 3).collect();
        s.push_str("...");
        s
    }
}
