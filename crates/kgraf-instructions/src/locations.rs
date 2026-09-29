//! Where each coding agent keeps its instructions and skills.

use crate::{Candidate, Scope};
use std::path::{Path, PathBuf};

/// What a location holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One markdown instruction file, such as `CLAUDE.md`.
    InstructionFile,
    /// A directory whose immediate subdirectories are skills.
    SkillsParent,
    /// One skill directory, holding a `SKILL.md`.
    SkillDir,
}

/// What a location's path is relative to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Home,
    Project,
}

/// One place an agent keeps something.
#[derive(Clone, Copy, Debug)]
pub struct Location {
    pub agent: &'static str,
    pub base: Base,
    /// Path relative to the base, with `/` separators.
    pub path: &'static str,
    pub kind: Kind,
    /// How well the location is known. "documented" means the agent's own
    /// documentation names it; "unverified" means it is believed but was not
    /// confirmed against that documentation for this table.
    pub confidence: &'static str,
}

/// Every location discovery checks, in the order layers apply within a scope.
pub const LOCATIONS: &[Location] = &[
    Location {
        agent: "claude-code",
        base: Base::Home,
        path: ".claude/CLAUDE.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "claude-code",
        base: Base::Home,
        path: ".claude/skills",
        kind: Kind::SkillsParent,
        confidence: "documented",
    },
    Location {
        agent: "codex",
        base: Base::Home,
        path: ".codex/AGENTS.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "codex",
        base: Base::Home,
        path: ".codex/skills",
        kind: Kind::SkillsParent,
        confidence: "unverified",
    },
    Location {
        agent: "gemini-cli",
        base: Base::Home,
        path: ".gemini/GEMINI.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "claude-code",
        base: Base::Project,
        path: "CLAUDE.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "claude-code",
        base: Base::Project,
        path: ".claude/CLAUDE.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "claude-code",
        base: Base::Project,
        path: "CLAUDE.local.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "claude-code",
        base: Base::Project,
        path: ".claude/skills",
        kind: Kind::SkillsParent,
        confidence: "documented",
    },
    Location {
        agent: "agents-md",
        base: Base::Project,
        path: "AGENTS.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "gemini-cli",
        base: Base::Project,
        path: "GEMINI.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
    Location {
        agent: "github-copilot",
        base: Base::Project,
        path: ".github/copilot-instructions.md",
        kind: Kind::InstructionFile,
        confidence: "documented",
    },
];

/// File names that are instruction sets wherever they appear in an imported folder.
const INSTRUCTION_FILE_NAMES: &[(&str, &str)] = &[
    ("CLAUDE.md", "claude-code"),
    ("CLAUDE.local.md", "claude-code"),
    ("AGENTS.md", "agents-md"),
    ("GEMINI.md", "gemini-cli"),
    ("copilot-instructions.md", "github-copilot"),
];

/// Directories never descended into when scanning a folder.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];

/// How deep a folder scan goes. Deep enough for `a/.claude/skills/x/SKILL.md`, and
/// bounded so a folder pointed at a home directory does not walk the whole disk.
const MAX_SCAN_DEPTH: usize = 8;

pub(crate) fn discover(home: Option<&Path>, project: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    // User scope first, then project: the order layers apply.
    for (scope, base_kind) in [(Scope::User, Base::Home), (Scope::Project, Base::Project)] {
        let base = match base_kind {
            Base::Home => match home {
                Some(home) => home,
                None => continue,
            },
            Base::Project => project,
        };
        for location in LOCATIONS.iter().filter(|l| l.base == base_kind) {
            let path = base.join(location.path);
            let exists = match location.kind {
                Kind::InstructionFile => path.is_file(),
                Kind::SkillsParent | Kind::SkillDir => path.is_dir(),
            };
            if exists {
                found.push(Candidate {
                    agent: location.agent.to_owned(),
                    scope,
                    kind: location.kind,
                    path,
                });
            }
        }
    }
    found
}

pub(crate) fn scan_folder(folder: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    let walker = walkdir::WalkDir::new(folder)
        .max_depth(MAX_SCAN_DEPTH)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && SKIPPED_DIRS
                    .iter()
                    .any(|skip| entry.file_name() == std::ffi::OsStr::new(skip)))
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name == "SKILL.md" {
            if let Some(dir) = entry.path().parent() {
                found.push(Candidate {
                    agent: "imported".to_owned(),
                    scope: Scope::Imported,
                    kind: Kind::SkillDir,
                    path: dir.to_owned(),
                });
            }
        } else if let Some((_, agent)) = INSTRUCTION_FILE_NAMES.iter().find(|(n, _)| *n == name) {
            found.push(Candidate {
                agent: (*agent).to_owned(),
                scope: Scope::Imported,
                kind: Kind::InstructionFile,
                path: entry.path().to_owned(),
            });
        }
    }
    found
}

/// The skill directories directly under a skills parent, sorted.
pub(crate) fn skill_dirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("SKILL.md").is_file())
        .collect();
    dirs.sort();
    dirs
}
