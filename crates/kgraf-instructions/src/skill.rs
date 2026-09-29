//! `SKILL.md` skills, in the Agent Skills format.

use crate::{Candidate, Problem, Source};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Longest skill name the format allows.
const MAX_NAME_CHARS: usize = 64;
/// Longest description the format allows.
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// One skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Every frontmatter key, as parsed, including ones this crate does not act on.
    pub frontmatter: serde_json::Value,
    /// The markdown after the frontmatter.
    pub body: String,
    /// Bundled files, relative to the skill directory, sorted. `SKILL.md` excluded.
    pub files: Vec<String>,
    /// Tools the skill restricts a worker to, where it declares any. Enforced by the
    /// runtime, never by the prompt (decision 0053).
    pub allowed_tools: Option<Vec<String>>,
    /// The model may not invoke this skill itself.
    pub disable_model_invocation: bool,
    /// The user may invoke this skill directly.
    pub user_invocable: bool,
    /// blake3 over `SKILL.md` and every bundled file, with their relative paths.
    pub hash: String,
    pub sources: Vec<Source>,
}

/// Read one skill directory.
///
/// A skill that cannot be identified (no frontmatter, no name, no description) is an
/// error returned as a [`Problem`]. One that can be identified but breaks a rule of
/// the format is returned with its problems listed, so the user sees both the skill
/// and what is wrong with it.
pub(crate) fn read(candidate: &Candidate) -> Result<(Skill, Vec<Problem>), Problem> {
    let dir = &candidate.path;
    let skill_md = dir.join("SKILL.md");
    let problem = |reason: String| Problem {
        path: skill_md.clone(),
        reason,
    };
    let text =
        std::fs::read_to_string(&skill_md).map_err(|e| problem(format!("cannot read: {e}")))?;
    let (yaml, body) = split_frontmatter(&text)
        .ok_or_else(|| problem("no YAML frontmatter between `---` lines".to_owned()))?;
    let frontmatter: serde_json::Value = serde_yaml_ng::from_str(yaml)
        .map_err(|e| problem(format!("frontmatter is not valid YAML: {e}")))?;
    let map = frontmatter
        .as_object()
        .ok_or_else(|| problem("frontmatter is not a mapping".to_owned()))?;

    let mut problems = Vec::new();
    let name = map
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| problem("frontmatter has no `name`".to_owned()))?;
    let description = map
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| problem(format!("skill `{name}` has no `description`")))?;

    if let Some(reason) = name_violation(&name) {
        problems.push(problem(format!("skill name `{name}` {reason}")));
    }
    if dir.file_name().and_then(|n| n.to_str()) != Some(name.as_str()) {
        problems.push(problem(format!(
            "skill name `{name}` does not match its directory name"
        )));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        problems.push(problem(format!(
            "description is longer than {MAX_DESCRIPTION_CHARS} characters"
        )));
    }

    let allowed_tools = match map.get("allowed-tools") {
        None => None,
        Some(serde_json::Value::String(s)) => Some(split_tools(s)),
        Some(serde_json::Value::Array(items)) => {
            let tools: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if tools.len() != items.len() {
                problems.push(problem("`allowed-tools` has a non-string entry".to_owned()));
            }
            Some(tools)
        }
        Some(_) => {
            problems.push(problem(
                "`allowed-tools` is neither a string nor a list".to_owned(),
            ));
            // Declared but unreadable restricts to nothing, not to everything: a
            // restriction the author meant must not silently widen.
            Some(Vec::new())
        }
    };
    let flag = |key: &str, default: bool, problems: &mut Vec<Problem>| match map.get(key) {
        None => default,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => {
            problems.push(problem(format!("`{key}` is not true or false")));
            default
        }
    };
    let disable_model_invocation = flag("disable-model-invocation", false, &mut problems);
    let user_invocable = flag("user-invocable", true, &mut problems);

    let files = bundled_files(dir);
    let hash = skill_hash(dir, &text, &files).map_err(problem)?;

    Ok((
        Skill {
            name,
            description,
            frontmatter,
            body: body.to_owned(),
            files,
            allowed_tools,
            disable_model_invocation,
            user_invocable,
            hash,
            sources: vec![Source {
                agent: candidate.agent.clone(),
                scope: candidate.scope,
                path: dir.clone(),
            }],
        },
        problems,
    ))
}

/// The YAML between a leading `---` line and the next `---` line, and the rest.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Why a name breaks the format's rules, if it does.
fn name_violation(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Some("must be 1 to 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Some("may use only lowercase letters, digits and hyphens");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Some("may not start or end with a hyphen or contain two in a row");
    }
    None
}

/// Split an `allowed-tools` string. Commas separate where present; otherwise
/// whitespace does, which is how the format writes it (`Read Bash(git:*)`).
fn split_tools(s: &str) -> Vec<String> {
    let parts: Vec<&str> = if s.contains(',') {
        s.split(',').collect()
    } else {
        s.split_whitespace().collect()
    };
    parts
        .into_iter()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

fn bundled_files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(dir)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .filter(|rel| rel != "SKILL.md")
        .collect();
    files.sort();
    files
}

fn skill_hash(dir: &Path, skill_md: &str, files: &[String]) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    let mut add = |rel: &str, bytes: &[u8]| {
        hasher.update(&(rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    add("SKILL.md", skill_md.as_bytes());
    for rel in files {
        let bytes = std::fs::read(dir.join(rel)).map_err(|e| format!("cannot read {rel}: {e}"))?;
        add(rel, &bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
