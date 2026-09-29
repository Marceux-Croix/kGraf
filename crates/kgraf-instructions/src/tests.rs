use super::*;
use std::fs;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, text).expect("write");
    path
}

fn skill_md(name: &str, description: &str, extra: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n{extra}---\n{body}")
}

fn build_folder(folder: &Path) -> Inventory {
    build(&scan_folder(folder), &BTreeMap::new(), None)
}

#[test]
fn a_folder_with_a_claude_md_and_skills_is_fully_accounted_for() {
    let dir = TempDir::new().expect("tmp");
    let root = dir.path();
    write(root, "CLAUDE.md", "# Rules\nAlways run the tests.\n");
    write(
        root,
        "skills/deploy/SKILL.md",
        &skill_md(
            "deploy",
            "Deploys the app.",
            "allowed-tools: Bash(git:*) Read\n",
            "Steps.\n",
        ),
    );
    write(root, "skills/deploy/scripts/run.sh", "echo hi\n");
    write(
        root,
        "skills/review/SKILL.md",
        &skill_md(
            "review",
            "Reviews a diff.",
            "disable-model-invocation: true\n",
            "Look.\n",
        ),
    );

    let inventory = build_folder(root);
    assert!(inventory.problems.is_empty(), "{:?}", inventory.problems);
    assert_eq!(inventory.layers.len(), 1);
    assert_eq!(inventory.layers[0].text, "# Rules\nAlways run the tests.\n");
    assert_eq!(inventory.layers[0].source.agent, "claude-code");

    let names: Vec<&str> = inventory.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["deploy", "review"]);
    let deploy = &inventory.skills[0];
    assert_eq!(deploy.files, vec!["scripts/run.sh"]);
    assert_eq!(
        deploy.allowed_tools.as_deref(),
        Some(&["Bash(git:*)".to_owned(), "Read".to_owned()][..])
    );
    assert!(!deploy.disable_model_invocation);
    assert!(inventory.skills[1].disable_model_invocation);
}

#[test]
fn identical_skills_in_two_places_merge_and_keep_both_sources() {
    let dir = TempDir::new().expect("tmp");
    let text = skill_md("fmt", "Formats code.", "", "Run fmt.\n");
    write(dir.path(), "a/fmt/SKILL.md", &text);
    write(dir.path(), "b/fmt/SKILL.md", &text);
    let inventory = build_folder(dir.path());
    assert!(inventory.conflicts.is_empty());
    assert_eq!(inventory.skills.len(), 1);
    assert_eq!(inventory.skills[0].sources.len(), 2);
}

#[test]
fn different_skills_with_one_name_are_a_conflict_until_the_user_chooses() {
    let dir = TempDir::new().expect("tmp");
    write(
        dir.path(),
        "a/fmt/SKILL.md",
        &skill_md("fmt", "Formats code.", "", "Run cargo fmt.\n"),
    );
    write(
        dir.path(),
        "b/fmt/SKILL.md",
        &skill_md("fmt", "Formats code.", "", "Run prettier.\n"),
    );
    let candidates = scan_folder(dir.path());

    let undecided = build(&candidates, &BTreeMap::new(), None);
    assert!(
        undecided.skills.is_empty(),
        "a conflicted skill must not be usable"
    );
    assert_eq!(undecided.conflicts.len(), 1);
    assert_eq!(undecided.conflicts[0].variants.len(), 2);
    assert_eq!(undecided.conflicts[0].chosen, None);

    let pick = undecided.conflicts[0].variants[1].hash.clone();
    let mut choices = BTreeMap::new();
    choices.insert("fmt".to_owned(), pick[..12].to_owned());
    let decided = build(&candidates, &choices, None);
    assert_eq!(decided.skills.len(), 1);
    assert_eq!(decided.skills[0].hash, pick);
    assert_eq!(decided.conflicts[0].chosen.as_deref(), Some(pick.as_str()));
}

#[test]
fn an_ambiguous_choice_chooses_nothing_and_says_so() {
    let dir = TempDir::new().expect("tmp");
    write(
        dir.path(),
        "a/fmt/SKILL.md",
        &skill_md("fmt", "Formats.", "", "one\n"),
    );
    write(
        dir.path(),
        "b/fmt/SKILL.md",
        &skill_md("fmt", "Formats.", "", "two\n"),
    );
    let mut choices = BTreeMap::new();
    choices.insert("fmt".to_owned(), String::new()); // matches every hash
    let inventory = build(&scan_folder(dir.path()), &choices, None);
    assert!(inventory.skills.is_empty());
    assert!(
        inventory
            .problems
            .iter()
            .any(|p| p.reason.contains("does not match exactly one variant"))
    );
}

#[test]
fn malformed_skills_are_reported_not_skipped() {
    let dir = TempDir::new().expect("tmp");
    write(
        dir.path(),
        "nofront/SKILL.md",
        "Just text, no frontmatter.\n",
    );
    write(
        dir.path(),
        "badyaml/SKILL.md",
        "---\nname: [unclosed\n---\nbody\n",
    );
    write(
        dir.path(),
        "nodesc/SKILL.md",
        "---\nname: nodesc\n---\nbody\n",
    );
    write(
        dir.path(),
        "Wrong_Name/SKILL.md",
        &skill_md("Wrong_Name", "Bad name.", "", "b\n"),
    );
    write(
        dir.path(),
        "flags/SKILL.md",
        &skill_md(
            "flags",
            "Bad flag.",
            "user-invocable: sometimes\nallowed-tools: 7\n",
            "b\n",
        ),
    );
    let inventory = build_folder(dir.path());
    let reasons: Vec<String> = inventory
        .problems
        .iter()
        .map(|p| format!("{}: {}", p.path.display(), p.reason))
        .collect();
    let has = |needle: &str| reasons.iter().any(|r| r.contains(needle));
    assert!(has("no YAML frontmatter"), "{reasons:#?}");
    assert!(has("not valid YAML"), "{reasons:#?}");
    assert!(has("has no `description`"), "{reasons:#?}");
    assert!(has("may use only lowercase"), "{reasons:#?}");
    assert!(has("`user-invocable` is not true or false"), "{reasons:#?}");
    assert!(has("`allowed-tools` is neither"), "{reasons:#?}");

    // The identifiable-but-flawed skills are still listed, so the user sees them.
    let names: Vec<&str> = inventory.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["Wrong_Name", "flags"]);
    // An unreadable restriction restricts to nothing rather than widening.
    let flags = &inventory.skills[1];
    assert_eq!(flags.allowed_tools.as_deref(), Some(&[][..]));
}

#[test]
fn a_skill_whose_name_differs_from_its_directory_is_flagged() {
    let dir = TempDir::new().expect("tmp");
    write(
        dir.path(),
        "folder/SKILL.md",
        &skill_md("other", "Mismatch.", "", "b\n"),
    );
    let inventory = build_folder(dir.path());
    assert!(
        inventory
            .problems
            .iter()
            .any(|p| p.reason.contains("does not match its directory name"))
    );
}

#[test]
fn imports_are_followed_in_order_and_broken_ones_reported() {
    let dir = TempDir::new().expect("tmp");
    let home = TempDir::new().expect("home");
    write(home.path(), "shared/style.md", "Use British spelling.\n");
    write(dir.path(), "docs/extra.md", "Extra rules.\n");
    write(
        dir.path(),
        "CLAUDE.md",
        "Main.\n@docs/extra.md\nSee @~/shared/style.md.\n@missing/file.md\n\
         Email me @someone and `@ignored/in/code.md`\n```\n@ignored/in/fence.md\n```\n",
    );
    let candidates = scan_folder(dir.path());
    let inventory = build(&candidates, &BTreeMap::new(), Some(home.path()));
    let texts: Vec<&str> = inventory.layers.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts[1], "Extra rules.\n");
    assert_eq!(texts[2], "Use British spelling.\n");
    assert_eq!(inventory.layers.len(), 3);
    assert!(inventory.layers[1].imported_by.is_some());
    let problems: Vec<&str> = inventory
        .problems
        .iter()
        .map(|p| p.reason.as_str())
        .collect();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("@missing/file.md"), "{problems:?}");
}

#[test]
fn an_import_cycle_terminates_and_is_reported() {
    let dir = TempDir::new().expect("tmp");
    write(dir.path(), "CLAUDE.md", "A\n@b.md\n");
    write(dir.path(), "b.md", "B\n@CLAUDE.md\n");
    let inventory = build_folder(dir.path());
    assert_eq!(inventory.layers.len(), 2);
    // The cycle is caught either as a repeat of content already layered or as a
    // cycle; neither reads a file twice.
    let texts: Vec<&str> = inventory.layers.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["A\n@b.md\n", "B\n@CLAUDE.md\n"]);
}

#[test]
fn byte_identical_instruction_files_are_one_layer_with_both_paths() {
    let dir = TempDir::new().expect("tmp");
    write(dir.path(), "CLAUDE.md", "Same rules.\n");
    write(dir.path(), "AGENTS.md", "Same rules.\n");
    let inventory = build_folder(dir.path());
    assert_eq!(inventory.layers.len(), 1);
    assert_eq!(inventory.layers[0].also_at.len(), 1);
}

#[test]
fn discovery_orders_user_scope_before_project_scope() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    write(home.path(), ".claude/CLAUDE.md", "global\n");
    write(
        home.path(),
        ".claude/skills/g/SKILL.md",
        &skill_md("g", "Global skill.", "", "b\n"),
    );
    write(project.path(), "CLAUDE.md", "project\n");
    write(project.path(), "AGENTS.md", "agents\n");
    let candidates = discover(Some(home.path()), project.path());
    let scopes: Vec<Scope> = candidates.iter().map(|c| c.scope).collect();
    assert_eq!(
        scopes,
        vec![Scope::User, Scope::User, Scope::Project, Scope::Project]
    );
    let inventory = build(&candidates, &BTreeMap::new(), Some(home.path()));
    let texts: Vec<&str> = inventory.layers.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["global\n", "project\n", "agents\n"]);
    assert_eq!(inventory.skills.len(), 1);
}

#[test]
fn a_rerun_over_unchanged_input_writes_nothing() {
    let dir = TempDir::new().expect("tmp");
    write(dir.path(), "CLAUDE.md", "rules\n");
    let out = dir.path().join("out/inventory.json");
    let first = serde_json::to_string_pretty(&build_folder(dir.path())).expect("json");
    write_if_changed(&out, &first).expect("write");
    let modified = fs::metadata(&out).expect("meta").modified().expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = serde_json::to_string_pretty(&build_folder(dir.path())).expect("json");
    assert_eq!(first, second, "the inventory must be deterministic");
    write_if_changed(&out, &second).expect("write");
    assert_eq!(
        fs::metadata(&out).expect("meta").modified().expect("mtime"),
        modified
    );
}

#[test]
fn config_round_trips_and_imports_are_recorded_once() {
    let dir = TempDir::new().expect("tmp");
    let path = dir.path().join("sources.json");
    let mut config = Config::load(&path).expect("absent is empty");
    config.add_import(PathBuf::from("/x"));
    config.add_import(PathBuf::from("/x"));
    config.choices.insert("fmt".into(), "abc".into());
    config.save(&path).expect("save");
    let back = Config::load(&path).expect("load");
    assert_eq!(back.imported, vec![PathBuf::from("/x")]);
    assert_eq!(back, config);
}
