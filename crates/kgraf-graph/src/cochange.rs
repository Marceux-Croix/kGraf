//! The co-change relation.
//!
//! Decision 0020: **the code graph is not the collision predictor.** An early
//! experiment falsified that, measuring a call graph's disjointness verdict as close to
//! statistically independent of whether two changes actually collide. Co-change
//! is the relation instead, and kGraf computes it itself from version history
//! rather than consuming another tool's thresholded version of it.
//!
//! Two rules from that record are enforced here.
//!
//! **Every observed pair is kept, with its count as a weight. Nothing is
//! thresholded at index time.** The reference implementation's thresholds discard
//! 99.96 percent of the relation and leave five percent file coverage;
//! unthresholded, the same repository gives 99 percent coverage in 19,341 edges,
//! 3.5 times smaller than the call graph it already carried. A value discarded
//! here cannot be recovered by a consumer that wanted another operating point, so
//! filters (commit size, recency, minimum count) are [`CoChangeQuery`]
//! parameters applied at read time.
//!
//! **Sources union and degrade gracefully.** Git is read regardless of who
//! authored the commits, so a user switching between kGraf and another
//! harness keeps the file-level relation and loses only task attribution for that
//! window. kGraf's own observation is strictly richer, because it records
//! per-task write sets and, uniquely, observed collisions, which git
//! structurally cannot contain, since review resolves the conflict before it
//! reaches a commit.

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use rusqlite::params;
use std::collections::BTreeMap;

/// One co-change pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoChangePair {
    /// Lexicographically first path, so a pair has one canonical row.
    pub path_a: String,
    pub path_b: String,
    /// How many commits changed both. The weight.
    pub count: u32,
    /// Unix seconds of the most recent commit containing the pair, so a consumer
    /// can weight by recency without the index having done it for them.
    pub last_commit_ts: i64,
    /// The size, in files, of the smallest commit that produced this pair.
    ///
    /// Recorded so commit-size filtering is a read-time choice. A pair seen only
    /// in a 713-file mega-commit is a different kind of evidence from one seen in
    /// a 3-file commit, and the measured pair counts move by an order of
    /// magnitude depending on the cap.
    pub min_commit_size: u32,
}

/// Coverage figures for a computed relation, which is how decision 0020's
/// threshold argument was made and is therefore worth reporting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoChangeStats {
    /// Commits read from history.
    pub commits: u64,
    /// Commits skipped because they exceeded the size cap.
    pub commits_skipped_oversized: u64,
    /// Distinct pairs stored.
    pub pairs: u64,
    /// Distinct files appearing in at least one stored pair.
    pub files_covered: u64,
    /// Distinct files appearing in history at all.
    pub files_in_history: u64,
}

impl CoChangeStats {
    /// The share of files in history that appear in at least one pair.
    ///
    /// This is the figure the threshold argument turns on: five percent with the
    /// reference's cuts, 99 percent without them.
    pub fn coverage(&self) -> f64 {
        if self.files_in_history == 0 {
            return 0.0;
        }
        self.files_covered as f64 / self.files_in_history as f64
    }
}

/// Read-time filters. Every one of these is a choice the consumer makes, and
/// none of them is applied when the relation is stored.
#[derive(Clone, Copy, Debug)]
pub struct CoChangeQuery {
    /// Minimum co-change count.
    pub min_count: u32,
    /// Ignore pairs seen only in commits larger than this many files. `None`
    /// keeps every pair. Mega-commits create spurious pairs across the whole
    /// tree, which is why this exists; it is off by default because the right
    /// value is not knowable here.
    pub max_commit_size: Option<u32>,
    /// Ignore pairs whose most recent commit is older than this many seconds.
    pub max_age_seconds: Option<i64>,
    /// Most pairs to return.
    pub limit: usize,
}

impl Default for CoChangeQuery {
    fn default() -> Self {
        Self {
            min_count: 1,
            max_commit_size: None,
            max_age_seconds: None,
            limit: 100,
        }
    }
}

/// One commit's file set, as read from history.
#[derive(Clone, Debug)]
pub struct HistoryCommit {
    /// Unix seconds.
    pub timestamp: i64,
    /// Repository-relative paths the commit changed.
    pub paths: Vec<String>,
}

/// Build the relation from a sequence of commits and store it.
///
/// `oversize_cap` bounds which commits *contribute pairs at all*, and is a
/// different thing from [`CoChangeQuery::max_commit_size`]: a commit touching
/// hundreds of files relates every pair of them, which is quadratic in both time
/// and rows, so a cap here is a tractability bound rather than an operating
/// point. It defaults high enough that no realistic hand-written commit is
/// excluded, and every excluded commit is counted in the returned stats so the
/// exclusion is visible rather than silent.
pub fn build_from_commits(
    store: &GraphStore,
    commits: &[HistoryCommit],
    oversize_cap: u32,
) -> Result<CoChangeStats> {
    let mut stats = CoChangeStats::default();
    let mut pairs: BTreeMap<(String, String), (u32, i64, u32)> = BTreeMap::new();
    let mut files_in_history = std::collections::BTreeSet::new();

    for commit in commits {
        stats.commits += 1;
        let mut paths: Vec<&String> = commit.paths.iter().collect();
        paths.sort();
        paths.dedup();
        for path in &paths {
            files_in_history.insert((*path).clone());
        }
        let size = paths.len() as u32;
        if size > oversize_cap {
            stats.commits_skipped_oversized += 1;
            continue;
        }
        for i in 0..paths.len() {
            for j in (i + 1)..paths.len() {
                let key = (paths[i].clone(), paths[j].clone());
                let entry = pairs.entry(key).or_insert((0, i64::MIN, u32::MAX));
                entry.0 += 1;
                entry.1 = entry.1.max(commit.timestamp);
                entry.2 = entry.2.min(size);
            }
        }
    }

    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM cochange", [])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO cochange(path_a, path_b, count, last_commit_ts, min_commit_size)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for ((a, b), (count, ts, min_size)) in &pairs {
            insert.execute(params![a, b, count, ts, min_size])?;
        }
    }
    tx.commit()?;

    let mut covered = std::collections::BTreeSet::new();
    for (a, b) in pairs.keys() {
        covered.insert(a.clone());
        covered.insert(b.clone());
    }
    stats.pairs = pairs.len() as u64;
    stats.files_covered = covered.len() as u64;
    stats.files_in_history = files_in_history.len() as u64;
    Ok(stats)
}

/// Read history from a git repository by shelling out to `git log`.
///
/// Shelling out rather than linking a git library: the relation is computed in
/// about two seconds over two thousand commits, so this is not a component that
/// needs its own performance budget, and a missing or non-git directory must
/// raise rather than silently yielding an empty relation (fail loud).
pub fn read_git_history(root: &std::path::Path, max_commits: u32) -> Result<Vec<HistoryCommit>> {
    if max_commits == 0 {
        // `--max-count=0` prints nothing and succeeds: an empty relation that
        // reads as "no history" (fail loud).
        return Err(GraphError::History {
            reason: "the commit limit is 0, which reads no history".to_owned(),
        });
    }
    // `-z` ends each path with NUL and prints it raw. Without it git quotes a
    // path holding non-ASCII bytes (`"\303\251.rs"`), and the quoted text would
    // be stored as if it were the file's name.
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("log")
        .arg("-z")
        .arg(format!("--max-count={max_commits}"))
        .arg("--no-merges")
        .arg("--name-only")
        .arg("--pretty=format:%x01%ct")
        .arg("--no-renames")
        .output()
        .map_err(|e| GraphError::History {
            reason: format!("cannot run git: {e}"),
        })?;
    if !output.status.success() {
        return Err(GraphError::History {
            reason: format_reason(&output),
        });
    }
    parse_git_log_z(&output.stdout)
}

/// Parse `git log -z --name-only --pretty=format:%x01%ct` output. A NUL ends
/// each path (and separates commits with an empty token); a header token starts
/// with `\x01`, holds the commit time, and is followed after a newline by that
/// commit's first path.
fn parse_git_log_z(stdout: &[u8]) -> Result<Vec<HistoryCommit>> {
    let bad = |what: String| GraphError::History { reason: what };
    let mut commits = Vec::new();
    let mut current: Option<HistoryCommit> = None;
    let finish = |commit: Option<HistoryCommit>, commits: &mut Vec<HistoryCommit>| {
        if let Some(commit) = commit
            && !commit.paths.is_empty()
        {
            commits.push(commit);
        }
    };
    for token in stdout.split(|b| *b == 0) {
        if token.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(token).map_err(|_| {
            bad(format!(
                "git log printed a path that is not valid UTF-8: {}",
                String::from_utf8_lossy(token)
            ))
        })?;
        let mut path = text;
        if let Some(header) = text.strip_prefix('\u{1}') {
            finish(current.take(), &mut commits);
            let (stamp, first) = match header.split_once('\n') {
                Some((stamp, first)) => (stamp, first),
                None => (header, ""),
            };
            let timestamp = stamp
                .trim()
                .parse::<i64>()
                .map_err(|_| bad(format!("git log printed a bad commit time `{stamp}`")))?;
            current = Some(HistoryCommit {
                timestamp,
                paths: Vec::new(),
            });
            path = first;
        }
        if path.is_empty() {
            continue;
        }
        match current.as_mut() {
            Some(commit) => commit.paths.push(path.to_owned()),
            None => {
                return Err(bad(format!(
                    "git log printed the path `{path}` before any commit header"
                )));
            }
        }
    }
    finish(current, &mut commits);
    Ok(commits)
}

fn format_reason(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("git log failed: {}", stderr.trim())
}

/// Pairs involving `path`, most co-changed first, under the given filters.
pub fn partners_of(
    store: &GraphStore,
    path: &str,
    query: CoChangeQuery,
    now: i64,
) -> Result<Vec<CoChangePair>> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT path_a, path_b, count, last_commit_ts, min_commit_size
         FROM cochange
         WHERE (path_a = ?1 OR path_b = ?1)
           AND count >= ?2
           AND (?3 IS NULL OR min_commit_size <= ?3)
           AND (?4 IS NULL OR last_commit_ts >= ?4)
         ORDER BY count DESC, last_commit_ts DESC
         LIMIT ?5",
    )?;
    let cutoff = query.max_age_seconds.map(|age| now - age);
    let rows = stmt.query_map(
        params![
            path,
            query.min_count,
            query.max_commit_size,
            cutoff,
            query.limit as i64
        ],
        |row| {
            Ok(CoChangePair {
                path_a: row.get(0)?,
                path_b: row.get(1)?,
                count: row.get(2)?,
                last_commit_ts: row.get(3)?,
                min_commit_size: row.get(4)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record a worker's observed write set for a task.
///
/// Strictly richer than git: task-level rather than squashed, and it includes
/// work later reverted or discarded, which is precisely what review strips out of
/// history.
pub fn record_task_writes(
    store: &GraphStore,
    task_id: &str,
    paths: &[String],
    observed_at: i64,
) -> Result<()> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO task_writes(task_id, path, observed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(task_id, path) DO UPDATE SET observed_at = excluded.observed_at",
        )?;
        for path in paths {
            insert.execute(params![task_id, path, observed_at])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Record an observed collision between two tasks on one path.
///
/// First-class because collisions are the training signal, and because git
/// cannot contain them.
pub fn record_collision(
    store: &GraphStore,
    task_a: &str,
    task_b: &str,
    path: &str,
    kind: &str,
    observed_at: i64,
) -> Result<()> {
    // Canonical order so the same collision cannot be recorded twice under
    // swapped task ids.
    let (first, second) = if task_a <= task_b {
        (task_a, task_b)
    } else {
        (task_b, task_a)
    };
    let id = kgraf_core::ids::stable_id("collision", &[first, second, path, kind]);
    store.conn().execute(
        "INSERT INTO collisions(id, task_a, task_b, path, kind, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET observed_at = excluded.observed_at",
        params![id, first, second, path, kind, observed_at],
    )?;
    Ok(())
}

/// How many collisions have been observed.
pub fn collision_count(store: &GraphStore) -> Result<u64> {
    Ok(store
        .conn()
        .query_row("SELECT COUNT(*) FROM collisions", [], |row| {
            row.get::<_, i64>(0)
        })? as u64)
}

/// Fold kGraf's own observed write sets into the stored relation.
///
/// Sources union: a task that wrote two files is evidence of the same kind a
/// commit touching two files is, and this is how the relation keeps improving
/// on a repository whose history is thin.
pub fn merge_observed_writes(store: &GraphStore) -> Result<u64> {
    let conn = store.conn();
    let mut stmt =
        conn.prepare("SELECT task_id, path, observed_at FROM task_writes ORDER BY task_id, path")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut by_task: BTreeMap<String, (Vec<String>, i64)> = BTreeMap::new();
    for row in rows {
        let (task, path, at) = row?;
        let entry = by_task.entry(task).or_insert((Vec::new(), 0));
        entry.0.push(path);
        entry.1 = entry.1.max(at);
    }
    let tx = conn.unchecked_transaction()?;
    let mut merged = 0u64;
    {
        let mut upsert = tx.prepare(
            "INSERT INTO cochange(path_a, path_b, count, last_commit_ts, min_commit_size)
             VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(path_a, path_b) DO UPDATE SET
                 count = count + 1,
                 last_commit_ts = MAX(last_commit_ts, excluded.last_commit_ts),
                 min_commit_size = MIN(min_commit_size, excluded.min_commit_size)",
        )?;
        for (paths, at) in by_task.values() {
            let mut sorted = paths.clone();
            sorted.sort();
            sorted.dedup();
            let size = sorted.len() as u32;
            for i in 0..sorted.len() {
                for j in (i + 1)..sorted.len() {
                    upsert.execute(params![sorted[i], sorted[j], at, size])?;
                    merged += 1;
                }
            }
        }
    }
    tx.commit()?;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        (dir, store)
    }

    fn commit(ts: i64, paths: &[&str]) -> HistoryCommit {
        HistoryCommit {
            timestamp: ts,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn every_observed_pair_is_kept_with_its_count_as_a_weight() {
        let (_dir, store) = store();
        let commits = vec![
            commit(100, &["a.rs", "b.rs"]),
            commit(200, &["a.rs", "b.rs"]),
            commit(300, &["a.rs", "c.rs"]),
        ];
        let stats = build_from_commits(&store, &commits, 500).expect("build");
        assert_eq!(stats.pairs, 2);
        let partners = partners_of(&store, "a.rs", CoChangeQuery::default(), 1000).expect("query");
        let ab = partners
            .iter()
            .find(|p| p.path_b == "b.rs" || p.path_a == "b.rs")
            .expect("a-b pair");
        assert_eq!(ab.count, 2);
        let ac = partners
            .iter()
            .find(|p| p.path_b == "c.rs" || p.path_a == "c.rs")
            .expect("a-c pair");
        assert_eq!(ac.count, 1, "a single-observation pair must survive");
    }

    #[test]
    fn a_single_observation_pair_is_not_thresholded_away() {
        // This is the reference implementation's behavior written as a test of
        // the opposite. Its cuts leave five percent file coverage; keeping every
        // pair gives near-total coverage, and the difference is whether a
        // consumer can pick its own operating point at all.
        let (_dir, store) = store();
        let mut commits = Vec::new();
        for i in 0..50 {
            commits.push(commit(i, &["hot_a.rs", "hot_b.rs"]));
        }
        for i in 0..20 {
            // Twenty pairs each seen exactly once, which every threshold at or
            // above two would discard.
            let a = format!("cold_{i}_a.rs");
            let b = format!("cold_{i}_b.rs");
            commits.push(HistoryCommit {
                timestamp: 1000 + i,
                paths: vec![a, b],
            });
        }
        let stats = build_from_commits(&store, &commits, 500).expect("build");
        assert_eq!(stats.pairs, 21);
        assert!(
            (stats.coverage() - 1.0).abs() < f64::EPSILON,
            "coverage was {}",
            stats.coverage()
        );

        // A consumer that *wants* a threshold applies it at read time, and gets
        // the tighter set without the index having thrown anything away.
        let tight = partners_of(
            &store,
            "cold_0_a.rs",
            CoChangeQuery {
                min_count: 2,
                ..CoChangeQuery::default()
            },
            2000,
        )
        .expect("query");
        assert!(tight.is_empty());
        let loose =
            partners_of(&store, "cold_0_a.rs", CoChangeQuery::default(), 2000).expect("query");
        assert_eq!(loose.len(), 1, "the pair is still there to be asked for");
    }

    #[test]
    fn commit_size_is_a_read_time_filter_not_an_index_time_cut() {
        let (_dir, store) = store();
        let mega: Vec<String> = (0..40).map(|i| format!("f{i}.rs")).collect();
        let commits = vec![
            HistoryCommit {
                timestamp: 100,
                paths: mega,
            },
            commit(200, &["f0.rs", "f99.rs"]),
        ];
        build_from_commits(&store, &commits, 500).expect("build");

        let unfiltered = partners_of(
            &store,
            "f0.rs",
            CoChangeQuery {
                limit: 1000,
                ..Default::default()
            },
            300,
        )
        .expect("query");
        assert!(unfiltered.len() > 30, "got {}", unfiltered.len());

        let small_only = partners_of(
            &store,
            "f0.rs",
            CoChangeQuery {
                max_commit_size: Some(5),
                limit: 1000,
                ..Default::default()
            },
            300,
        )
        .expect("query");
        assert_eq!(
            small_only.len(),
            1,
            "only the 2-file commit's pair should survive: {small_only:#?}"
        );
    }

    #[test]
    fn recency_is_available_as_a_read_time_weighting() {
        let (_dir, store) = store();
        let commits = vec![
            commit(100, &["old_a.rs", "old_b.rs"]),
            commit(9_000, &["new_a.rs", "new_b.rs"]),
        ];
        build_from_commits(&store, &commits, 500).expect("build");
        let recent = partners_of(
            &store,
            "old_a.rs",
            CoChangeQuery {
                max_age_seconds: Some(1_000),
                ..Default::default()
            },
            10_000,
        )
        .expect("query");
        assert!(recent.is_empty(), "a two-year-old pair must be filterable");
    }

    #[test]
    fn an_oversized_commit_is_excluded_visibly_not_silently() {
        let (_dir, store) = store();
        let mega: Vec<String> = (0..100).map(|i| format!("f{i}.rs")).collect();
        let stats = build_from_commits(
            &store,
            &[HistoryCommit {
                timestamp: 1,
                paths: mega,
            }],
            10,
        )
        .expect("build");
        assert_eq!(stats.commits_skipped_oversized, 1);
        assert_eq!(stats.pairs, 0);
        // The files are still counted as seen, so coverage reports honestly
        // rather than dividing by the files that happened to produce pairs.
        assert_eq!(stats.files_in_history, 100);
        assert_eq!(stats.coverage(), 0.0);
    }

    #[test]
    fn observed_write_sets_union_into_the_relation() {
        let (_dir, store) = store();
        build_from_commits(&store, &[commit(100, &["a.rs", "b.rs"])], 500).expect("build");
        record_task_writes(&store, "task-1", &["a.rs".into(), "b.rs".into()], 500).expect("record");
        record_task_writes(&store, "task-2", &["b.rs".into(), "z.rs".into()], 600).expect("record");
        merge_observed_writes(&store).expect("merge");

        let partners = partners_of(&store, "a.rs", CoChangeQuery::default(), 1000).expect("query");
        let ab = partners.iter().find(|p| p.path_b == "b.rs").expect("a-b");
        assert_eq!(ab.count, 2, "git plus one observed task write");

        // A pair git never saw is now in the relation.
        let z = partners_of(&store, "z.rs", CoChangeQuery::default(), 1000).expect("query");
        assert_eq!(z.len(), 1);
    }

    #[test]
    fn collisions_are_recorded_once_regardless_of_task_order() {
        let (_dir, store) = store();
        record_collision(&store, "b", "a", "shared.rs", "textual", 1).expect("record");
        record_collision(&store, "a", "b", "shared.rs", "textual", 2).expect("record");
        assert_eq!(
            collision_count(&store).expect("count"),
            1,
            "the same collision must not be double counted under swapped ids"
        );
    }

    #[test]
    fn git_history_is_read_from_a_real_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git");
            assert!(status.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("a.rs"), "fn a2() {}\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "second"]);

        let commits = read_git_history(dir.path(), 100).expect("history");
        assert_eq!(commits.len(), 2);
        let two_file = commits
            .iter()
            .find(|c| c.paths.len() == 2)
            .expect("the first commit touched two files");
        assert!(two_file.paths.contains(&"a.rs".to_owned()));
    }

    #[test]
    fn non_ascii_paths_are_read_as_themselves_not_as_git_quoting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("caf\u{e9}.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir.path().join("with space.rs"), "fn b() {}\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "first"]);
        let commits = read_git_history(dir.path(), 10).expect("history");
        assert_eq!(commits.len(), 1);
        let mut paths = commits[0].paths.clone();
        paths.sort();
        assert_eq!(paths, ["caf\u{e9}.rs", "with space.rs"]);
    }

    #[test]
    fn a_commit_limit_of_zero_is_an_error() {
        // A real repository with history, so the zero is the only thing wrong.
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "first"]);
        assert_eq!(read_git_history(dir.path(), 1).expect("one").len(), 1);
        let err = read_git_history(dir.path(), 0).expect_err("must raise");
        assert!(matches!(err, GraphError::History { .. }), "got {err:?}");
    }

    #[test]
    fn the_log_parser_reads_nul_separated_paths_and_refuses_a_bad_time() {
        let good = b"\x011700000000\na.rs\0b/c.rs\0\0\x011700000100\nd.rs\0";
        let commits = parse_git_log_z(good).expect("parse");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].timestamp, 1_700_000_000);
        assert_eq!(commits[0].paths, ["a.rs", "b/c.rs"]);
        assert_eq!(commits[1].paths, ["d.rs"]);

        let bad = parse_git_log_z(b"\x01notatime\na.rs\0").expect_err("bad time");
        assert!(matches!(bad, GraphError::History { .. }), "{bad:?}");
        let orphan = parse_git_log_z(b"a.rs\0").expect_err("path before header");
        assert!(matches!(orphan, GraphError::History { .. }), "{orphan:?}");
        let raw = parse_git_log_z(b"\x011\n\xff\xfe.rs\0").expect_err("non utf-8");
        assert!(matches!(raw, GraphError::History { .. }), "{raw:?}");
    }

    #[test]
    fn a_directory_that_is_not_a_repository_raises() {
        // Fail loud: an empty relation and a missing repository must not look the
        // same, since one of the founding failures was a missing directory
        // reading as "nothing yet".
        let dir = tempfile::tempdir().expect("tempdir");
        let err = read_git_history(dir.path(), 10).expect_err("must raise");
        assert!(matches!(err, GraphError::History { .. }), "got {err:?}");
    }
}
