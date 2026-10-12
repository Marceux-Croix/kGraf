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
    let frontmatter: serde_json::Value = serde_saphyr::from_str(yaml)
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

    let files = bundled_files(dir).map_err(problem)?;
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

/// The files bundled with a skill, relative to its directory. An entry the walk
/// cannot read is an error naming it: leaving it out would drop that file from
/// the bundle hash, and two skills that differ only there would read as one. A
/// symbolic link is a bundled entry too: it is listed, and hashed by its target
/// text without being followed.
fn bundled_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.map_err(|e| {
            let at = e
                .path()
                .map_or_else(|| dir.display().to_string(), |p| p.display().to_string());
            format!("cannot read {at} in the skill bundle: {e}")
        })?;
        if !entry.file_type().is_file() && !entry.path_is_symlink() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel != "SKILL.md" {
            files.push(rel);
        }
    }
    files.sort();
    Ok(files)
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
        let full = dir.join(rel);
        let is_link = std::fs::symlink_metadata(&full)
            .map_err(|e| format!("cannot read {rel}: {e}"))?
            .file_type()
            .is_symlink();
        if is_link {
            // The link's own text, never the file it points at, so a link that
            // dangles or points outside the bundle still hashes.
            let target =
                std::fs::read_link(&full).map_err(|e| format!("cannot read {rel}: {e}"))?;
            add(
                rel,
                format!("symlink:{}", target.to_string_lossy()).as_bytes(),
            );
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|e| format!("cannot read {rel}: {e}"))?;
        add(rel, &bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Scope};

    fn skill_dir(root: &Path) -> std::path::PathBuf {
        let dir = root.join("demo");
        std::fs::create_dir_all(dir.join("refs")).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo\ndescription: A demo.\n---\nBody\n",
        )
        .expect("skill");
        std::fs::write(dir.join("refs/a.md"), "a").expect("bundled");
        dir
    }

    fn candidate(dir: &Path) -> Candidate {
        Candidate {
            agent: "test".to_owned(),
            scope: Scope::Project,
            kind: Kind::SkillDir,
            path: dir.to_path_buf(),
        }
    }

    #[test]
    fn a_readable_bundle_lists_and_hashes_its_files() {
        let root = tempfile::tempdir().expect("dir");
        let dir = skill_dir(root.path());
        let (skill, _) = read(&candidate(&dir)).expect("read");
        assert_eq!(skill.files, ["refs/a.md"]);
        std::fs::write(dir.join("refs/a.md"), "b").expect("edit");
        let (edited, _) = read(&candidate(&dir)).expect("read");
        assert_ne!(skill.hash, edited.hash);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_bundle_is_listed_and_changes_the_hash() {
        let root = tempfile::tempdir().expect("dir");
        let dir = skill_dir(root.path());
        let (plain, _) = read(&candidate(&dir)).expect("read");
        std::os::unix::fs::symlink("a.md", dir.join("refs/link.md")).expect("link");
        let (linked, _) = read(&candidate(&dir)).expect("read");
        assert_eq!(linked.files, ["refs/a.md", "refs/link.md"]);
        assert_ne!(plain.hash, linked.hash, "the link was left out of the hash");
        // Re-pointing it changes the hash; a dangling target still hashes.
        std::fs::remove_file(dir.join("refs/link.md")).expect("rm");
        std::os::unix::fs::symlink("gone.md", dir.join("refs/link.md")).expect("link");
        let (repointed, _) = read(&candidate(&dir)).expect("dangling link still reads");
        assert_ne!(linked.hash, repointed.hash);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_in_the_bundle_is_an_error_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("dir");
        let dir = skill_dir(root.path());
        let refs = dir.join("refs");
        std::fs::set_permissions(&refs, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let unreadable = std::fs::read_dir(&refs).is_err();
        let result = read(&candidate(&dir));
        std::fs::set_permissions(&refs, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        if !unreadable {
            return; // running as a user who ignores permissions
        }
        let problem = result.expect_err("a bundle that cannot be read is not hashed");
        assert!(problem.reason.contains("refs"), "{}", problem.reason);
    }

    fn read_with_frontmatter(frontmatter: &str) -> Result<(Skill, Vec<Problem>), Problem> {
        let root = tempfile::tempdir().expect("dir");
        let dir = skill_dir(root.path());
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\nBody\n"),
        )
        .expect("skill");
        read(&candidate(&dir))
    }

    #[test]
    fn frontmatter_anchors_and_aliases_resolve() {
        let (skill, _) =
            read_with_frontmatter("name: demo\nbase: &d A demo.\ndescription: *d\n").expect("read");
        assert_eq!(skill.description, "A demo.");
    }

    #[test]
    fn frontmatter_with_a_tab_indent_is_not_valid_yaml() {
        let problem = read_with_frontmatter("name: demo\nextra:\n\t- a\ndescription: x\n")
            .expect_err("a tab cannot indent YAML");
        assert!(
            problem.reason.contains("not valid YAML"),
            "{}",
            problem.reason
        );
    }

    #[test]
    fn empty_frontmatter_is_refused_not_a_panic() {
        let problem = read_with_frontmatter("").expect_err("no mapping");
        assert!(!problem.reason.is_empty());
    }
}
