//! Instruction files in the style of `CLAUDE.md`, and the files they import.

use crate::{Candidate, Problem, Source, hash_hex};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How many `@import` hops are followed. Matches the depth Claude Code documents for
/// its own memory imports, so a file that works there is read the same way here.
const MAX_IMPORT_DEPTH: usize = 5;

/// One instruction file, as it will be layered into a worker's context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionLayer {
    pub source: Source,
    /// Content hash, full blake3 hex.
    pub hash: String,
    pub text: String,
    /// The file whose `@import` brought this one in, if any.
    pub imported_by: Option<PathBuf>,
    /// Other paths with byte-identical content. They are not repeated as layers,
    /// because the same text twice in a context is waste, but their provenance is kept.
    pub also_at: Vec<PathBuf>,
}

/// Read one instruction file, then the files it imports, depth first, in order.
pub(crate) fn read_with_imports(
    candidate: &Candidate,
    home: Option<&Path>,
    seen: &mut BTreeMap<String, usize>,
    layers: &mut Vec<InstructionLayer>,
    problems: &mut Vec<Problem>,
) {
    let mut chain = Vec::new();
    read_one(
        candidate,
        &candidate.path,
        None,
        home,
        0,
        &mut chain,
        seen,
        layers,
        problems,
    );
}

#[allow(clippy::too_many_arguments)]
fn read_one(
    candidate: &Candidate,
    path: &Path,
    imported_by: Option<&Path>,
    home: Option<&Path>,
    depth: usize,
    chain: &mut Vec<PathBuf>,
    seen: &mut BTreeMap<String, usize>,
    layers: &mut Vec<InstructionLayer>,
    problems: &mut Vec<Problem>,
) {
    let canonical = kgraf_core::plain_path::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if chain.contains(&canonical) {
        problems.push(Problem {
            path: path.to_owned(),
            reason: "import cycle; not read again".to_owned(),
        });
        return;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            problems.push(Problem {
                path: path.to_owned(),
                reason: format!("cannot read: {e}"),
            });
            return;
        }
    };
    let hash = hash_hex(text.as_bytes());
    match seen.get(&hash) {
        Some(&index) => {
            let layer = &mut layers[index];
            if layer.source.path != path && !layer.also_at.iter().any(|p| p == path) {
                layer.also_at.push(path.to_owned());
            }
            // Its imports were already followed from the first copy.
            return;
        }
        None => {
            seen.insert(hash.clone(), layers.len());
            layers.push(InstructionLayer {
                source: Source {
                    agent: candidate.agent.clone(),
                    scope: candidate.scope,
                    path: path.to_owned(),
                },
                hash,
                text: text.clone(),
                imported_by: imported_by.map(Path::to_owned),
                also_at: Vec::new(),
            });
        }
    }

    let references = import_references(&text);
    if references.is_empty() {
        return;
    }
    if depth >= MAX_IMPORT_DEPTH {
        problems.push(Problem {
            path: path.to_owned(),
            reason: format!("imports deeper than {MAX_IMPORT_DEPTH} hops are not followed"),
        });
        return;
    }
    chain.push(canonical);
    let dir = path.parent().unwrap_or(Path::new("."));
    for reference in references {
        let Some(target) = resolve(&reference, dir, home) else {
            problems.push(Problem {
                path: path.to_owned(),
                reason: format!("@{reference}: cannot resolve `~` without a home directory"),
            });
            continue;
        };
        if target.is_file() {
            read_one(
                candidate,
                &target,
                Some(path),
                home,
                depth + 1,
                chain,
                seen,
                layers,
                problems,
            );
        } else {
            problems.push(Problem {
                path: path.to_owned(),
                reason: format!(
                    "@{reference} imports {}, which does not exist",
                    target.display()
                ),
            });
        }
    }
    chain.pop();
}

/// The `@path` references in a file, outside code.
///
/// A reference is a whitespace-delimited token starting with `@` that looks like a
/// path: it contains a `/` or ends in `.md`. That excludes a bare `@mention`. Tokens in
/// fenced code blocks and inline code spans are ignored, as they are examples, not
/// imports.
pub(crate) fn import_references(text: &str) -> Vec<String> {
    let mut references = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut outside_code = String::new();
        for (i, segment) in line.split('`').enumerate() {
            if i % 2 == 0 {
                outside_code.push_str(segment);
                outside_code.push(' ');
            }
        }
        for token in outside_code.split_whitespace() {
            let Some(rest) = token.strip_prefix('@') else {
                continue;
            };
            let rest = rest.trim_end_matches(['.', ',', ';', ':', ')', ']', '!', '?']);
            if !rest.is_empty() && (rest.contains('/') || rest.ends_with(".md")) {
                references.push(rest.to_owned());
            }
        }
    }
    references
}

fn resolve(reference: &str, dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = reference.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    let path = Path::new(reference);
    Some(if path.is_absolute() {
        path.to_owned()
    } else {
        dir.join(path)
    })
}
