//! Ranking what matters in a repository: aider's repo-map PageRank, ported.
//!
//! **Source (reuse existing code).** `aider/repomap.py`, `RepoMap.get_ranked_tags`, Aider-AI/aider
//! at commit 94bd7abd17cdbcdab91b445b3ab165ecd2220938 (the last commit touching the
//! file, 2026-02-11; read 2026-09-26), Apache-2.0. The license text and the port
//! record are in `third_party/aider/`. The PageRank itself is networkx's
//! `pagerank` (BSD-3-Clause), which aider calls; it is reimplemented here as the
//! same power iteration and checked against networkx 3.7's output to 1e-9 (see the
//! `matches_networkx_reference` test and the script recorded in the UPSTREAM file).
//!
//! **What is ported, step by step.**
//!
//! 1. Nodes are files. Every file that defines an identifier and every file that
//!    references one is a node.
//! 2. For each identifier both defined and referenced, an edge runs from each
//!    referencing file to each defining file, weighted `mul * sqrt(refs)`, where
//!    `mul` starts at 1 and is x10 when the identifier was mentioned (in the
//!    instruction), x10 when it is snake, kebab or camel case and at least eight
//!    characters long, x0.1 when it starts with `_`, x0.1 when more than five files
//!    define it, and x50 when the referencing file is a focus file.
//! 3. An identifier that is defined and never referenced gets a 0.1 self-edge on
//!    each definer.
//! 4. When nothing at all is referenced, the definitions stand in for references.
//! 5. PageRank (alpha 0.85, 100 iterations, tolerance 1e-6 per node) with a
//!    personalization of `100 / files` for each focus file, mentioned file, and file
//!    with a path component equal to a mentioned identifier; the same vector is the
//!    dangling distribution. A personalization that sums to zero over the graph's
//!    nodes falls back to the uniform one, as aider's `ZeroDivisionError` handler
//!    does.
//! 6. Each file's rank is split across its out-edges in proportion to weight and
//!    credited to `(defining file, identifier)`; definitions sort by that credit,
//!    descending, ties broken by `(path, identifier)` descending (Python's
//!    `sorted(..., reverse=True)` on the tuple). Focus files' own definitions are
//!    left out of the ranked list, as aider leaves out chat files.
//!
//! **Where this differs from aider, and why.**
//!
//! - *References come from the graph, not from tree-sitter tag queries.* A
//!   reference is a row of `call_sites` (a call to a name, recorded at index time,
//!   resolved or not); a definition is a node other than a file, a Markdown section
//!   or an `impl` block (an `impl` is named for the type it implements, so counting
//!   it would define every type twice). Aider's tag queries also count non-call
//!   references (a type named in a signature); the graph does not record those, so
//!   types rank lower here than in aider. Callers may add references from outside
//!   the code graph (a document naming a symbol) through
//!   [`RankRequest::extra_references`].
//! - *`sqrt` is applied once per referencing file.* Aider reassigns
//!   `num_refs = math.sqrt(num_refs)` inside the loop over definers, so with several
//!   definers the second gets the fourth root, the third the eighth, in Python set
//!   iteration order, which varies from run to run with string hashing. The comment
//!   beside it ("scale down so high freq (low value) mentions don't dominate")
//!   states the intent, one square root; that is what this port does, and it makes
//!   the ranking deterministic.
//! - *A power iteration that does not converge is an error* (`RankDidNotConverge`),
//!   as networkx raises `PowerIterationFailedConvergence`; aider does not catch it
//!   either.
//! - The token-bounded map (`get_ranked_tags_map_uncached`'s binary search over
//!   how many tags fit) is the caller's job: the interpretation pass fills a token
//!   budget from this ranked list.

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// networkx's default damping factor, which aider uses.
pub const ALPHA: f64 = 0.85;
/// networkx's default iteration cap.
pub const MAX_ITER: usize = 100;
/// networkx's default tolerance, per node.
pub const TOL: f64 = 1.0e-6;

/// What to rank toward.
#[derive(Clone, Debug, Default)]
pub struct RankRequest {
    /// Files being worked on (aider's chat files): personalized, their
    /// references weigh x50, and their own definitions are left out of the list.
    pub focus_files: BTreeSet<String>,
    /// Files named in the instruction or the documents: personalized.
    pub mentioned_files: BTreeSet<String>,
    /// Identifiers named in the instruction: their edges weigh x10, and a file
    /// whose path has a component equal to one is personalized.
    pub mentioned_idents: BTreeSet<String>,
    /// References from outside the code graph, as (referencing path, identifier):
    /// a design document naming a function references it as a caller does.
    pub extra_references: Vec<(String, String)>,
}

/// One definition and the rank credited to it.
#[derive(Clone, Debug, PartialEq)]
pub struct RankedDefinition {
    pub path: String,
    pub ident: String,
    pub rank: f64,
}

/// The ranking of a repository.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoRank {
    /// Definitions, most important first.
    pub definitions: Vec<RankedDefinition>,
    /// Files in the ranked graph with their PageRank, most important first.
    pub files: Vec<(String, f64)>,
}

/// networkx `pagerank` on a weighted multigraph given as `(src, dst, weight)`
/// over nodes `0..n`: parallel edges add, each row is normalized by its out-weight,
/// dangling mass goes to `personalization` (or uniformly), and the iteration stops
/// when the L1 change is below `n * TOL`.
///
/// `personalization` need not be normalized. `None`, or one summing to zero, is
/// uniform (aider's fallback); one of the wrong length is an error, never a
/// silent uniform. An empty graph ranks nothing.
pub fn pagerank(
    n: usize,
    edges: &[(usize, usize, f64)],
    personalization: Option<&[f64]>,
) -> Result<Vec<f64>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let uniform = vec![1.0 / n as f64; n];
    if let Some(v) = personalization
        && v.len() != n
    {
        return Err(GraphError::RankPersonalization {
            nodes: n,
            given: v.len(),
        });
    }
    let p: Vec<f64> = match personalization {
        Some(v) if v.iter().sum::<f64>() > 0.0 => {
            let s: f64 = v.iter().sum();
            v.iter().map(|x| x / s).collect()
        }
        _ => uniform.clone(),
    };
    let mut out_weight = vec![0.0f64; n];
    for &(s, _, w) in edges {
        out_weight[s] += w;
    }
    let dangling: Vec<usize> = (0..n).filter(|&i| out_weight[i] == 0.0).collect();
    let mut x = uniform;
    let mut next = vec![0.0f64; n];
    for _ in 0..MAX_ITER {
        // x = alpha * (x @ A + sum(x[dangling]) * p) + (1 - alpha) * p
        let dangling_mass: f64 = dangling.iter().map(|&i| x[i]).sum();
        next.fill(0.0);
        for &(s, d, w) in edges {
            next[d] += x[s] * w / out_weight[s];
        }
        let mut err = 0.0;
        for i in 0..n {
            let v = ALPHA * (next[i] + dangling_mass * p[i]) + (1.0 - ALPHA) * p[i];
            err += (v - x[i]).abs();
            next[i] = v;
        }
        std::mem::swap(&mut x, &mut next);
        if err < n as f64 * TOL {
            return Ok(x);
        }
    }
    Err(GraphError::RankDidNotConverge {
        iterations: MAX_ITER,
    })
}

/// Aider's identifier multiplier, before the focus-file x50.
fn ident_multiplier(ident: &str, mentioned: &BTreeSet<String>, definers: usize) -> f64 {
    let has_alpha = ident.chars().any(char::is_alphabetic);
    let is_snake = ident.contains('_') && has_alpha;
    let is_kebab = ident.contains('-') && has_alpha;
    let is_camel = ident.chars().any(char::is_uppercase) && ident.chars().any(char::is_lowercase);
    let mut mul = 1.0;
    if mentioned.contains(ident) {
        mul *= 10.0;
    }
    if (is_snake || is_kebab || is_camel) && ident.chars().count() >= 8 {
        mul *= 10.0;
    }
    if ident.starts_with('_') {
        mul *= 0.1;
    }
    if definers > 5 {
        mul *= 0.1;
    }
    mul
}

/// Whether any path component, the file name, or the file stem equals a
/// mentioned identifier (aider's `components_to_check`).
fn path_mentions(path: &str, idents: &BTreeSet<String>) -> bool {
    if idents.is_empty() {
        return false;
    }
    let p = std::path::Path::new(path);
    let stem = p.file_stem().and_then(|s| s.to_str());
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .chain(stem)
        .any(|c| idents.contains(c))
}

/// One labeled edge of the multigraph.
struct Edge {
    src: usize,
    dst: usize,
    weight: f64,
    ident: String,
}

/// Rank the indexed repository's definitions (see the module documentation).
pub fn rank_repository(store: &GraphStore, request: &RankRequest) -> Result<RepoRank> {
    // defines[ident] = files defining it; references[ident] = referencing files, one
    // entry per reference (aider's list, counted with `Counter`).
    let mut defines: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut references: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut stmt = store.conn().prepare(
            "SELECT n.name, f.path FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE n.kind NOT IN ('file', 'section', 'impl') AND n.name <> ''",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (name, path) = row?;
            defines.entry(name).or_default().insert(path);
        }
        let mut stmt = store.conn().prepare(
            "SELECT c.callee_name, f.path FROM call_sites c
             JOIN nodes n ON n.id = c.caller_id JOIN files f ON f.id = n.file_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (name, path) = row?;
            references.entry(name).or_default().push(path);
        }
    }
    for (path, ident) in &request.extra_references {
        references
            .entry(ident.clone())
            .or_default()
            .push(path.clone());
    }
    // Aider: "if not references: references = defines".
    if references.is_empty() {
        references = defines
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
            .collect();
    }

    // Every file aider would consider: the indexed files plus outside referencers.
    let mut fnames: BTreeSet<String> = store.file_paths()?.into_iter().collect();
    fnames.extend(request.extra_references.iter().map(|(p, _)| p.clone()));
    if fnames.is_empty() {
        return Ok(RepoRank::default());
    }
    let personalize = 100.0 / fnames.len() as f64;
    let mut personalization: BTreeMap<&str, f64> = BTreeMap::new();
    for f in &fnames {
        let mut pers = 0.0;
        if request.focus_files.contains(f) {
            pers += personalize;
        }
        if request.mentioned_files.contains(f) {
            pers = f64::max(pers, personalize);
        }
        if path_mentions(f, &request.mentioned_idents) {
            pers += personalize;
        }
        if pers > 0.0 {
            personalization.insert(f.as_str(), pers);
        }
    }

    // The multigraph, nodes in insertion order as networkx keeps them.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let mut node = |path: &str| -> usize {
        if let Some(&i) = index.get(path) {
            return i;
        }
        let i = names.len();
        index.insert(path.to_owned(), i);
        names.push(path.to_owned());
        i
    };
    let mut edges: Vec<Edge> = Vec::new();
    for (ident, definers) in &defines {
        if references.contains_key(ident) {
            continue;
        }
        for d in definers {
            let i = node(d);
            edges.push(Edge {
                src: i,
                dst: i,
                weight: 0.1,
                ident: ident.clone(),
            });
        }
    }
    for (ident, refs) in &references {
        let Some(definers) = defines.get(ident) else {
            continue;
        };
        let mul = ident_multiplier(ident, &request.mentioned_idents, definers.len());
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for r in refs {
            *counts.entry(r.as_str()).or_default() += 1;
        }
        for (referencer, num_refs) in counts {
            let use_mul = if request.focus_files.contains(referencer) {
                mul * 50.0
            } else {
                mul
            };
            let weight = use_mul * (num_refs as f64).sqrt();
            let src = node(referencer);
            for definer in definers {
                let dst = node(definer);
                edges.push(Edge {
                    src,
                    dst,
                    weight,
                    ident: ident.clone(),
                });
            }
        }
    }

    let n = names.len();
    let pers: Vec<f64> = names
        .iter()
        .map(|f| personalization.get(f.as_str()).copied().unwrap_or(0.0))
        .collect();
    let plain: Vec<(usize, usize, f64)> = edges.iter().map(|e| (e.src, e.dst, e.weight)).collect();
    let ranked = pagerank(
        n,
        &plain,
        if personalization.is_empty() {
            None
        } else {
            Some(&pers)
        },
    )?;

    // Distribute each file's rank across its out-edges by weight.
    let mut out_weight = vec![0.0f64; n];
    for e in &edges {
        out_weight[e.src] += e.weight;
    }
    let mut credit: BTreeMap<(usize, &str), f64> = BTreeMap::new();
    for e in &edges {
        *credit.entry((e.dst, e.ident.as_str())).or_default() +=
            ranked[e.src] * e.weight / out_weight[e.src];
    }
    let mut definitions: Vec<RankedDefinition> = credit
        .into_iter()
        .filter(|((dst, _), _)| !request.focus_files.contains(&names[*dst]))
        .map(|((dst, ident), rank)| RankedDefinition {
            path: names[dst].clone(),
            ident: ident.to_owned(),
            rank,
        })
        .collect();
    definitions.sort_by(|a, b| {
        b.rank
            .total_cmp(&a.rank)
            .then_with(|| b.path.cmp(&a.path))
            .then_with(|| b.ident.cmp(&a.ident))
    });
    let mut files: Vec<(String, f64)> = names.into_iter().zip(ranked).collect();
    files.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    Ok(RepoRank { definitions, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// The fixture and expected values from the networkx 3.7 script recorded in
    /// `third_party/aider/UPSTREAM` (nodes a..f are indices 0..5).
    fn fixture() -> Vec<(usize, usize, f64)> {
        vec![
            (0, 1, 10.0 * 3f64.sqrt()),
            (0, 2, 1.0),
            (0, 2, 2.0),
            (1, 2, 2f64.sqrt()),
            (2, 0, 0.1),
            (3, 2, 100.0),
            (3, 3, 0.1),
            (4, 4, 0.1),
        ]
    }

    #[test]
    fn matches_networkx_reference() {
        let e = fixture();
        let plain = pagerank(5, &e, None).expect("rank");
        let want = [
            0.26724303717632175,
            0.22362012743881118,
            0.2791113392092933,
            0.03002549617557367,
            0.2,
        ];
        for (g, w) in plain.iter().zip(want) {
            assert!(close(*g, w), "plain {plain:?}");
        }
        let pers = [50.0, 0.0, 0.0, 0.0, 50.0];
        let got = pagerank(5, &e, Some(&pers)).expect("rank");
        let want = [
            0.20277549407690965,
            0.14691370837534912,
            0.15032341857338083,
            0.0,
            0.4999873789743604,
        ];
        for (g, w) in got.iter().zip(want) {
            assert!(close(*g, w), "personalized {got:?}");
        }
        // A dangling node (f, only a target).
        let mut e = e;
        e.push((4, 5, 2.0));
        let got = pagerank(6, &e, None).expect("rank");
        let want = [
            0.3014350704350956,
            0.25222916756740404,
            0.314817177769685,
            0.03386681675601385,
            0.035265470626102045,
            0.062386296845699574,
        ];
        for (g, w) in got.iter().zip(want) {
            assert!(close(*g, w), "dangling {got:?}");
        }
        let pers = [50.0, 0.0, 0.0, 0.0, 50.0, 0.0];
        let got = pagerank(6, &e, Some(&pers)).expect("rank");
        let want = [
            0.3161191286677593,
            0.22903057517161465,
            0.23434739480223882,
            0.0,
            0.12185686653960756,
            0.09864603481877944,
        ];
        for (g, w) in got.iter().zip(want) {
            assert!(close(*g, w), "dangling personalized {got:?}");
        }
    }

    #[test]
    fn a_zero_personalization_falls_back_to_uniform() {
        let e = fixture();
        let plain = pagerank(5, &e, None).expect("rank");
        let zero = pagerank(5, &e, Some(&[0.0; 5])).expect("rank");
        assert_eq!(plain, zero);
        assert!(pagerank(0, &[], None).expect("empty").is_empty());
        assert!(matches!(
            pagerank(5, &e, Some(&[1.0; 4])),
            Err(GraphError::RankPersonalization { nodes: 5, given: 4 })
        ));
    }

    #[test]
    fn multipliers_follow_aider() {
        let none = BTreeSet::new();
        assert!(close(ident_multiplier("x", &none, 1), 1.0));
        assert!(close(ident_multiplier("parse_all_files", &none, 1), 10.0));
        assert!(close(ident_multiplier("ParseAllFiles", &none, 1), 10.0));
        assert!(close(ident_multiplier("_x", &none, 1), 0.1));
        // Snake case and eight characters (x10), then the underscore (x0.1).
        assert!(close(ident_multiplier("_private", &none, 1), 1.0));
        assert!(close(ident_multiplier("new", &none, 6), 0.1));
        let m: BTreeSet<String> = ["new".to_owned()].into();
        assert!(close(ident_multiplier("new", &m, 1), 10.0));
        assert!(path_mentions(
            "crates/ledger/src/lib.rs",
            &["ledger".to_owned()].into()
        ));
        assert!(path_mentions(
            "src/ledger.rs",
            &["ledger".to_owned()].into()
        ));
        assert!(!path_mentions(
            "src/ledgers.rs",
            &["ledger".to_owned()].into()
        ));
    }

    fn write(root: &std::path::Path, path: &str, text: &str) {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, text).expect("write");
    }

    #[test]
    fn a_widely_referenced_file_outranks_its_callers_and_mentions_steer_the_rank() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        write(root, "src/core.rs", "pub fn shared_helper() {}\n");
        write(root, "src/leaf.rs", "pub fn lonely_function() {}\n");
        for i in 0..4 {
            write(
                root,
                &format!("src/user{i}.rs"),
                &format!("pub fn user{i}() {{ shared_helper(); }}\n"),
            );
        }
        let store = GraphStore::open_in_memory(root).expect("store");
        crate::Indexer::new(&store).index().expect("index");

        let rank = rank_repository(&store, &RankRequest::default()).expect("rank");
        let file_rank = |r: &RepoRank, path: &str| {
            r.files
                .iter()
                .find(|f| f.0 == path)
                .map(|f| f.1)
                .unwrap_or_else(|| panic!("{path} ranked: {:?}", r.files))
        };
        let pos = |r: &RepoRank, ident: &str| {
            r.definitions
                .iter()
                .position(|d| d.ident == ident)
                .unwrap_or_else(|| panic!("{ident} ranked: {:?}", r.definitions))
        };
        for i in 0..4 {
            let user = format!("src/user{i}.rs");
            assert!(file_rank(&rank, "src/core.rs") > file_rank(&rank, &user));
            assert!(pos(&rank, "shared_helper") < pos(&rank, &format!("user{i}")));
        }
        // Aider's 0.1 self-edge on an unreferenced definition makes its file keep
        // its own rank: the isolated leaf is not demoted, as upstream.
        assert!(pos(&rank, "lonely_function") < 2, "{:?}", rank.definitions);

        // An instruction mentioning `user0` and a document naming it twice lift it
        // above the leaf and every other caller: its file stem matches a mentioned
        // identifier (personalized), and the document's references carry a
        // mentioned identifier's x10. `shared_helper` stays first, because
        // `user0.rs` passes its own rank on to it.
        let steered = rank_repository(
            &store,
            &RankRequest {
                mentioned_idents: ["user0".to_owned()].into(),
                mentioned_files: ["docs/plan.md".to_owned()].into(),
                extra_references: vec![
                    ("docs/plan.md".to_owned(), "user0".to_owned()),
                    ("docs/plan.md".to_owned(), "user0".to_owned()),
                ],
                ..RankRequest::default()
            },
        )
        .expect("rank");
        assert!(pos(&rank, "user0") > 1, "{:?}", rank.definitions);
        assert_eq!(pos(&steered, "user0"), 1, "{:?}", steered.definitions);
        assert!(pos(&steered, "user0") < pos(&steered, "lonely_function"));
        // Ranks are a distribution over the files.
        let total: f64 = steered.files.iter().map(|f| f.1).sum();
        assert!((total - 1.0).abs() < 1e-6, "{total}");
    }
}
