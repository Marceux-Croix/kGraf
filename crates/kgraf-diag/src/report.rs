//! `kgraf diag show` and `kgraf diag export`: reading the file
//! back out, either as a human summary or as a shareable copy.

use crate::event::{DiagEvent, Record};
use crate::scrub;
use crate::store::{self, DiagError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A human-readable summary of `records`: usage counts, then errors newest
/// first. This is what `kgraf diag show` prints; it never re-reads the file
/// itself, so a caller can also use it on an already-loaded or already
/// re-scrubbed set of records (`export` does both).
pub fn render_summary(records: &[Record]) -> String {
    let mut launches = 0u64;
    let mut runs_started = 0u64;
    let mut runs_finished_ok = 0u64;
    let mut runs_finished_err = 0u64;
    let mut workers: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut features: BTreeMap<&String, u64> = BTreeMap::new();
    let mut errors: Vec<&Record> = Vec::new();
    let mut dropped = 0u64;
    // (registry, trigger) -> (fetches ok, fetches failed, packages downloaded)
    let mut fetches: BTreeMap<(String, String), (u64, u64, u64)> = BTreeMap::new();

    for r in records {
        match &r.event {
            DiagEvent::Launch { feature } => {
                launches += 1;
                if let Some(f) = feature {
                    *features.entry(f).or_default() += 1;
                }
            }
            DiagEvent::RunStarted { .. } => runs_started += 1,
            DiagEvent::RunFinished { ok, .. } => {
                if *ok {
                    runs_finished_ok += 1;
                } else {
                    runs_finished_err += 1;
                }
            }
            DiagEvent::WorkerSpawned { provider, model } => {
                *workers
                    .entry((provider.clone(), model.clone()))
                    .or_default() += 1;
            }
            DiagEvent::FeatureUsed { feature } => *features.entry(feature).or_default() += 1,
            DiagEvent::Error { .. } => errors.push(r),
            DiagEvent::Dropped { count } => dropped += count,
            DiagEvent::RegistryFetch {
                registry,
                trigger,
                ok,
                downloaded,
            } => {
                let e = fetches
                    .entry((registry.clone(), trigger.clone()))
                    .or_default();
                if *ok {
                    e.0 += 1;
                } else {
                    e.1 += 1;
                }
                e.2 += downloaded;
            }
        }
    }

    let mut out = String::new();
    out.push_str("kGraf local diagnostics\n");
    out.push_str("Nothing here is ever sent anywhere; this file lives only on this machine.\n\n");
    out.push_str(&format!("{} record(s)\n\n", records.len()));

    out.push_str("Usage\n");
    out.push_str(&format!("  launches: {launches}\n"));
    out.push_str(&format!("  runs started: {runs_started}\n"));
    out.push_str(&format!(
        "  runs finished: {runs_finished_ok} ok, {runs_finished_err} failed\n"
    ));
    if !workers.is_empty() {
        out.push_str("  workers spawned, by provider:model:\n");
        for ((provider, model), count) in &workers {
            out.push_str(&format!("    {provider}:{model}: {count}\n"));
        }
    }
    if !features.is_empty() {
        out.push_str("  feature use:\n");
        for (feature, count) in &features {
            out.push_str(&format!("    {feature}: {count}\n"));
        }
    }
    if !fetches.is_empty() {
        out.push_str("  registry fetches (network, no tokens), by registry and trigger:\n");
        for ((registry, trigger), (ok, failed, downloaded)) in &fetches {
            out.push_str(&format!(
                "    {registry} ({trigger}): {ok} ok, {failed} failed, {downloaded} package(s)\n"
            ));
        }
    }
    if dropped > 0 {
        out.push_str(&format!(
            "  dropped (recording channel full at some point): {dropped}\n"
        ));
    }

    out.push_str(&format!("\nErrors ({}), newest first\n", errors.len()));
    for r in errors.iter().rev() {
        if let DiagEvent::Error {
            code,
            component,
            context,
        } = &r.event
        {
            out.push_str(&format!(
                "  [{}] {code} ({component}): {context}\n",
                format_ts(r.ts_ms)
            ));
        }
    }
    out
}

fn format_ts(ts_ms: i64) -> String {
    // No `chrono` dependency here for one formatted field: this stays a plain
    // "seconds since epoch" render, which is what a diagnostics file is
    // grepped against anyway; a caller that wants a calendar date has the raw
    // `ts_ms` in the JSONL export.
    format!("t+{}s", ts_ms / 1000)
}

/// Read `path`, scrub every field again (covering anything that reached the file before a scrub bug was
/// fixed, or was recorded by a component that used its own `project_root`),
/// and write the result to `out` (or, when `out` is `None`, a
/// `kgraf-diag-export-<ts_ms>.jsonl` file in `path`'s own directory, not the
/// current directory: a caller running from an arbitrary working directory
/// must not litter it). Returns the path written to.
pub fn export(
    path: &Path,
    out: Option<PathBuf>,
    project_root: Option<&Path>,
) -> Result<PathBuf, DiagError> {
    let records = store::read_all(path)?;
    let scrubbed: Vec<Record> = records
        .into_iter()
        .map(|r| Record {
            ts_ms: r.ts_ms,
            event: rescrub_event(r.event, project_root),
        })
        .collect();
    let default_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let out_path = out.unwrap_or_else(|| {
        default_dir.join(format!(
            "kgraf-diag-export-{}.jsonl",
            crate::event::now_ms()
        ))
    });
    let mut body = String::new();
    for r in &scrubbed {
        body.push_str(&serde_json::to_string(r).unwrap_or_default());
        body.push('\n');
    }
    std::fs::write(&out_path, body).map_err(|source| DiagError::Write {
        path: out_path.clone(),
        source,
    })?;
    Ok(out_path)
}

/// Re-scrub every string field of `event`, not only `Error::context`:
/// `run`, `feature`, `provider` and `model`
/// are meant to already be safe (a run id, a fixed feature name, a catalog
/// route and model name), but a defensive re-scrub costs nothing when the
/// string was already clean (`scrub::scrub_text` is a no-op on text with
/// nothing path-shaped in it) and closes the gap if a future caller ever
/// passes something less controlled into one of them.
fn rescrub_event(event: DiagEvent, project_root: Option<&Path>) -> DiagEvent {
    let s = |text: String| scrub::scrub_text(&text, project_root);
    match event {
        DiagEvent::Error {
            code,
            component,
            context,
        } => DiagEvent::Error {
            code,
            component,
            context: s(context),
        },
        DiagEvent::RunStarted { run } => DiagEvent::RunStarted { run: s(run) },
        DiagEvent::RunFinished { run, ok } => DiagEvent::RunFinished { run: s(run), ok },
        DiagEvent::WorkerSpawned { provider, model } => DiagEvent::WorkerSpawned {
            provider: s(provider),
            model: s(model),
        },
        DiagEvent::FeatureUsed { feature } => DiagEvent::FeatureUsed {
            feature: s(feature),
        },
        DiagEvent::Launch { feature } => DiagEvent::Launch {
            feature: feature.map(s),
        },
        DiagEvent::RegistryFetch {
            registry,
            trigger,
            ok,
            downloaded,
        } => DiagEvent::RegistryFetch {
            registry: s(registry),
            trigger: s(trigger),
            ok,
            downloaded,
        },
        other @ DiagEvent::Dropped { .. } => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Record;

    #[test]
    fn summary_counts_each_kind() {
        let records = vec![
            // A CLI launch merges its feature into one record.
            Record::now(DiagEvent::launch_with_feature("orchestrate")),
            Record::now(DiagEvent::RunStarted { run: "r1".into() }),
            Record::now(DiagEvent::RunFinished {
                run: "r1".into(),
                ok: true,
            }),
            Record::now(DiagEvent::worker_spawned("kimi", "kimi-k2")),
            Record::now(DiagEvent::FeatureUsed {
                feature: "index".into(),
            }),
            Record::now(DiagEvent::error("KG-0501", "kgraf-host", "route not ready")),
        ];
        let summary = render_summary(&records);
        assert!(summary.contains("launches: 1"));
        assert!(summary.contains("runs started: 1"));
        assert!(summary.contains("kimi:kimi-k2: 1"));
        assert!(summary.contains("index: 1"));
        assert!(summary.contains("orchestrate: 1"));
        assert!(summary.contains("KG-0501"));
    }

    /// Decision 0087: a registry fetch is a network event with no tokens,
    /// and the summary shows it by registry and trigger, failures apart.
    #[test]
    fn summary_shows_registry_fetches() {
        let fetch = |trigger: &str, ok: bool, downloaded: u64| {
            Record::now(DiagEvent::RegistryFetch {
                registry: "crates.io".into(),
                trigger: trigger.into(),
                ok,
                downloaded,
            })
        };
        let records = vec![
            fetch("run-start", true, 12),
            fetch("run-start", false, 0),
            fetch("worker", true, 1),
        ];
        let summary = render_summary(&records);
        assert!(
            summary.contains("registry fetches (network, no tokens)"),
            "{summary}"
        );
        assert!(
            summary.contains("crates.io (run-start): 1 ok, 1 failed, 12 package(s)"),
            "{summary}"
        );
        assert!(
            summary.contains("crates.io (worker): 1 ok, 0 failed, 1 package(s)"),
            "{summary}"
        );
        assert!(!render_summary(&[]).contains("registry fetches"));
    }

    #[test]
    fn export_rescrubs_and_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("diag.jsonl");
        let root = dir.path().to_path_buf();
        let leaking = format!("cannot read `{}`", dir.path().join("secretish").display());
        store::append(
            &src,
            &Record::now(DiagEvent::error("KG-0201", "kgraf-graph", leaking)),
            store::DEFAULT_CAP_BYTES,
        )
        .unwrap();
        let out_path = dir.path().join("out.jsonl");
        export(&src, Some(out_path.clone()), Some(&root)).unwrap();
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("secretish"));
        assert!(!written.contains(&root.display().to_string()));
    }
}
