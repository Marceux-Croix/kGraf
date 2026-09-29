//! Instruction sets and skills, gathered from the agents a user already runs.
//!
//! Decision 0053. A user arriving at kGraf has usually spent months writing a
//! `CLAUDE.md`, an `AGENTS.md`, a `GEMINI.md` and a directory of `SKILL.md` skills for
//! other coding agents. This crate reads all of them into **one inventory**, so the
//! agent runtime can use them without the user rewriting anything.
//!
//! Three properties are deliberate.
//!
//! **Where each agent keeps its files is data, not code.** [`LOCATIONS`] is one table,
//! so supporting another agent is a row. Each row says how well its location is known,
//! because an agent's documented layout and a guessed one are not the same claim.
//!
//! **Nothing is silently chosen.** Two skills with the same name and different content
//! are a [`Conflict`], listed with every source. The skill is unusable until the user
//! picks a variant. Byte-identical copies found in several places are one skill with
//! several sources, which is a merge, not a choice.
//!
//! **Every problem is reported, not skipped.** A malformed frontmatter, a missing
//! description or an `@import` that points nowhere becomes a [`Problem`] naming the
//! file. The inventory is honest about what it could not read.

mod instructions;
mod locations;
mod skill;

pub use instructions::InstructionLayer;
pub use locations::{Base, Kind, LOCATIONS, Location};
pub use skill::Skill;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Failures that stop the inventory from being written at all.
///
/// Problems with individual files are not errors: they are collected into
/// [`Inventory::problems`] so one bad skill cannot hide the rest.
#[derive(Debug, thiserror::Error)]
pub enum InstructionsError {
    /// A file kGraf itself owns could not be read or written.
    #[error("cannot access {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// kGraf's own configuration file is not valid.
    #[error("cannot parse {path}")]
    Config {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Where a file came from, relative to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The user's own global configuration, under their home directory.
    User,
    /// The project being worked on.
    Project,
    /// A folder the user pointed the importer at.
    Imported,
}

/// One file or skill directory found, before it is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The agent the location belongs to, such as `claude-code`.
    pub agent: String,
    pub scope: Scope,
    pub kind: Kind,
    pub path: PathBuf,
}

/// Where one inventory entry was read from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Source {
    pub agent: String,
    pub scope: Scope,
    pub path: PathBuf,
}

/// A file that could not be read as what its location says it is.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Problem {
    pub path: PathBuf,
    pub reason: String,
}

/// Two or more different skills sharing one name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub name: String,
    /// Every variant, by content hash, with where each was found.
    pub variants: Vec<ConflictVariant>,
    /// The hash the user chose, if any. Until one is chosen the skill is absent
    /// from [`Inventory::skills`].
    pub chosen: Option<String>,
}

/// One variant of a conflicting skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictVariant {
    pub hash: String,
    pub description: String,
    pub sources: Vec<Source>,
}

/// Everything found, unified.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    /// Instruction layers in the order they apply: user-global first, then project,
    /// then imported folders, each followed by the files it imports.
    pub layers: Vec<InstructionLayer>,
    /// Usable skills, one per name, sorted by name.
    pub skills: Vec<Skill>,
    pub conflicts: Vec<Conflict>,
    pub problems: Vec<Problem>,
}

/// The user's choices and imported folders, kept between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Folders added with the import tool, in the order they were added.
    #[serde(default)]
    pub imported: Vec<PathBuf>,
    /// For a conflicting skill name, the content hash (or a unique prefix of it)
    /// the user chose.
    #[serde(default)]
    pub choices: BTreeMap<String, String>,
}

impl Config {
    /// Read the configuration, or an empty one if the file does not exist yet.
    pub fn load(path: &Path) -> Result<Self, InstructionsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| InstructionsError::Config {
                path: path.to_owned(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(InstructionsError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Write the configuration.
    pub fn save(&self, path: &Path) -> Result<(), InstructionsError> {
        let text =
            serde_json::to_string_pretty(self).map_err(|source| InstructionsError::Config {
                path: path.to_owned(),
                source,
            })?;
        write_if_changed(path, &text)
    }

    /// Record an imported folder, once.
    pub fn add_import(&mut self, folder: PathBuf) {
        if !self.imported.contains(&folder) {
            self.imported.push(folder);
        }
    }
}

/// Find every known location that exists, for a user and a project.
///
/// `home` is `None` where the platform has no home directory; the user-scope rows are
/// then skipped rather than guessed.
pub fn discover(home: Option<&Path>, project: &Path) -> Vec<Candidate> {
    locations::discover(home, project)
}

/// Find every instruction file and skill under a folder the user named.
pub fn scan_folder(folder: &Path) -> Vec<Candidate> {
    locations::scan_folder(folder)
}

/// Read and unify a set of candidates.
///
/// Candidates are read in the order given, which is the order instruction layers
/// apply. `choices` resolves conflicts the user has already decided.
pub fn build(
    candidates: &[Candidate],
    choices: &BTreeMap<String, String>,
    home: Option<&Path>,
) -> Inventory {
    let mut inventory = Inventory::default();
    let mut found_skills: Vec<Skill> = Vec::new();
    let mut seen_layers: BTreeMap<String, usize> = BTreeMap::new();

    for candidate in candidates {
        match candidate.kind {
            Kind::InstructionFile => {
                instructions::read_with_imports(
                    candidate,
                    home,
                    &mut seen_layers,
                    &mut inventory.layers,
                    &mut inventory.problems,
                );
            }
            Kind::SkillDir => match skill::read(candidate) {
                Ok((skill, problems)) => {
                    inventory.problems.extend(problems);
                    found_skills.push(skill);
                }
                Err(problem) => inventory.problems.push(problem),
            },
            Kind::SkillsParent => {
                for dir in locations::skill_dirs(&candidate.path) {
                    let child = Candidate {
                        kind: Kind::SkillDir,
                        path: dir,
                        ..candidate.clone()
                    };
                    match skill::read(&child) {
                        Ok((skill, problems)) => {
                            inventory.problems.extend(problems);
                            found_skills.push(skill);
                        }
                        Err(problem) => inventory.problems.push(problem),
                    }
                }
            }
        }
    }

    unify_skills(found_skills, choices, &mut inventory);
    inventory.problems.sort();
    inventory.problems.dedup();
    inventory
}

/// Group skills by name, merge identical ones, and report the rest as conflicts.
fn unify_skills(found: Vec<Skill>, choices: &BTreeMap<String, String>, inventory: &mut Inventory) {
    let mut by_name: BTreeMap<String, BTreeMap<String, Skill>> = BTreeMap::new();
    for skill in found {
        let variants = by_name.entry(skill.name.clone()).or_default();
        match variants.get_mut(&skill.hash) {
            Some(existing) => {
                existing.sources.extend(skill.sources);
                existing.sources.sort();
                existing.sources.dedup();
            }
            None => {
                variants.insert(skill.hash.clone(), skill);
            }
        }
    }

    for (name, mut variants) in by_name {
        if variants.len() == 1 {
            if let Some((_, skill)) = variants.pop_first() {
                inventory.skills.push(skill);
            }
            continue;
        }
        let chosen = choices.get(&name).and_then(|prefix| {
            let matching: Vec<&String> = variants
                .keys()
                .filter(|h| h.starts_with(prefix.as_str()))
                .collect();
            // A prefix matching two variants chooses nothing: picking the first would
            // be the silent choice this crate exists not to make.
            (matching.len() == 1).then(|| matching[0].clone())
        });
        if let (Some(prefix), None) = (choices.get(&name), &chosen) {
            inventory.problems.push(Problem {
                path: PathBuf::from(format!("<choice for skill {name}>")),
                reason: format!(
                    "the chosen hash prefix {prefix:?} does not match exactly one variant"
                ),
            });
        }
        inventory.conflicts.push(Conflict {
            name: name.clone(),
            variants: variants
                .values()
                .map(|skill| ConflictVariant {
                    hash: skill.hash.clone(),
                    description: skill.description.clone(),
                    sources: skill.sources.clone(),
                })
                .collect(),
            chosen: chosen.clone(),
        });
        if let Some(hash) = chosen
            && let Some(skill) = variants.remove(&hash)
        {
            inventory.skills.push(skill);
        }
    }
}

/// The full blake3 hash of some bytes, as hex.
pub(crate) fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Write `text` to `path` only if it differs, so a rerun over unchanged input
/// touches nothing.
pub fn write_if_changed(path: &Path, text: &str) -> Result<(), InstructionsError> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == text) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| InstructionsError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| InstructionsError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests;
