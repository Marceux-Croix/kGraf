//! `kgraf-wintest pack`: stage the bundle on the build machine.
//!
//! Copies each test binary to `tests/`, each program a test starts by path
//! to `programs/`, the listed source files to `src/`, and writes the
//! manifest. The packaging script adds the runner, the README and the
//! manual test text, then zips the folder.

use std::path::Path;

use crate::cli::PackOptions;
use crate::manifest::{self, MANIFEST_NAME, Manifest};

/// The top-level entries of a bundle folder.
pub const BUNDLE_ENTRIES: &[&str] = &[
    "kgraf-wintest.exe",
    "README.txt",
    "MANUAL-NETWORK-TEST.txt",
    MANIFEST_NAME,
    "tests",
    "programs",
    "src",
];

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))
}

pub fn pack(o: &PackOptions) -> Result<String, String> {
    let messages = std::fs::read_to_string(&o.messages).map_err(|e| e.to_string())?;
    let artifacts = manifest::artifacts_from_messages(&messages);
    let (runs, programs, not_shipped) = manifest::plan_from_messages(&artifacts, &o.build_root);
    let missing = manifest::missing_core(&runs);
    if !missing.is_empty() {
        return Err(format!(
            "the build has no binary for core entries {missing:?}; a renamed test must be renamed in manifest::CORE too"
        ));
    }
    let root = Path::new(&o.build_root);
    let mut copied = std::collections::BTreeSet::new();
    for r in &runs {
        if copied.insert(r.exe.clone()) {
            let a = artifacts
                .iter()
                .find(|a| {
                    a.is_test
                        && r.exe
                            .ends_with(a.executable.rsplit(['/', '\\']).next().unwrap_or(""))
                })
                .ok_or_else(|| format!("no artifact for {}", r.id))?;
            copy(Path::new(&a.executable), &o.out.join(&r.exe))?;
        }
    }
    for dir in ["tests", "programs", "src"] {
        std::fs::create_dir_all(o.out.join(dir)).map_err(|e| e.to_string())?;
    }
    for p in &programs {
        copy(&root.join(&p.build_path), &o.out.join(&p.exe))?;
    }
    let list = std::fs::read_to_string(&o.src_list).map_err(|e| e.to_string())?;
    let mut files = 0usize;
    for rel in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let from = root.join(rel);
        if from.is_file() {
            copy(&from, &o.out.join("src").join(rel))?;
            files += 1;
        }
    }
    let m = Manifest {
        schema: 1,
        kgraf_version: o.version.clone(),
        git_commit: o.commit.clone(),
        built_utc: o.built_utc.clone(),
        target: "x86_64-pc-windows-msvc".into(),
        build_root: o.build_root.clone(),
        runs,
        programs,
        not_shipped,
        bundle_entries: BUNDLE_ENTRIES.iter().map(|s| (*s).to_owned()).collect(),
    };
    let text = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
    std::fs::write(o.out.join(MANIFEST_NAME), text).map_err(|e| e.to_string())?;
    let distinct_exes = copied.len();
    let core = m
        .runs
        .iter()
        .filter(|r| r.set == manifest::Set::Core)
        .count();
    Ok(format!(
        "{} runs ({core} core) over {distinct_exes} test binaries, {} programs, {files} source files, {} binaries not shipped",
        m.runs.len(),
        m.programs.len(),
        m.not_shipped.len()
    ))
}
