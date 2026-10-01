//! The command line's side of the user's permission settings (decision 0094):
//! the user-level file, then the flags, into one live control.
//!
//! The file is the user's own (`<config dir>/kGraf/permissions.toml`, or
//! `KGRAF_PERMISSIONS_PATH`, or `--permissions-file`), refused when it lies
//! inside the project. The flags add to it: `--permission-mode` sets the mode,
//! `--allow` and `--allow-path` add to Auto's allow set (`--may-grant` is
//! `--allow`'s older name), `--deny` and `--ask` add rules, `--plan-review`
//! turns plan review on and `--approve-plan` approves the plan without
//! showing it. `--container-sandbox` is `--permission-mode bypass` for a run
//! inside a disposable container, and still needs `KGRAF_CONTAINER_SANDBOX=1`.

use anyhow::{Context, Result, bail};
use kgraf_tools::permissions::{
    AllowSet, ModeSource, PERMISSIONS_FILE_NAME, PERMISSIONS_PATH_ENV, PermissionControl,
    PermissionMode, PermissionSettings, ProcessFacts, Rule,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The environment variable `--container-sandbox` also requires (decision
/// 0093): a second, separate statement that the run's commands execute in a
/// disposable container, which the harness that made the container sets.
pub const CONTAINER_SANDBOX_ENV: &str = kgraf_tools::permissions::CONTAINER_STATEMENT_ENV;

/// Printed to stderr whenever a run starts in Bypass, so no log of the run
/// hides it.
pub const BYPASS_NOTICE: &str = "kgraf: BYPASS PERMISSION MODE: every permission request is \
granted without asking; only your deny and ask rules and the refusal to delete system paths \
still apply. It offers no protection against prompt injection or unintended actions.";

/// The permission flags of one command.
#[derive(Clone, Debug, Default)]
pub struct PermissionArgs {
    pub mode: Option<PermissionMode>,
    pub allow: Vec<String>,
    pub allow_paths: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
    pub container_sandbox: bool,
    pub plan_review: bool,
    pub permissions_file: Option<PathBuf>,
}

/// The user-level file this command reads: the flag, the environment
/// variable, or the per-user configuration directory.
pub fn user_file(flag: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(p.to_path_buf());
    }
    match std::env::var_os(PERMISSIONS_PATH_ENV) {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => directories::ProjectDirs::from("", "", kgraf_diag::paths::APP_NAME)
            .map(|d| d.config_dir().join(PERMISSIONS_FILE_NAME)),
    }
}

/// Whether `--container-sandbox` is on (decision 0093): the flag and
/// `KGRAF_CONTAINER_SANDBOX=1` together. The flag without the variable fails
/// loud; the variable alone turns nothing on.
pub fn container_sandbox(flag: bool, env: Option<&str>) -> Result<bool> {
    match (flag, env) {
        (false, _) => Ok(false),
        (true, Some("1")) => Ok(true),
        (true, _) => bail!(
            "--container-sandbox also needs {CONTAINER_SANDBOX_ENV}=1 in the environment: set it \
             only where this run's commands execute in a disposable container"
        ),
    }
}

/// The settings this command runs under: the file's, with the flags applied,
/// and where the mode came from. Pure over its inputs.
pub fn settings(
    file: PermissionSettings,
    args: &PermissionArgs,
    container_sandbox: bool,
) -> Result<(PermissionSettings, ModeSource)> {
    let mut s = file;
    s.allow = AllowSet {
        paths: s
            .allow
            .paths
            .iter()
            .cloned()
            .chain(args.allow_paths.iter().cloned())
            .collect(),
        ..s.allow
    }
    .with_programs(args.allow.iter().cloned())
    .map_err(anyhow::Error::msg)
    .context("--allow")?;
    for (list, flag, out) in [
        (&args.deny, "--deny", &mut s.rules.deny),
        (&args.ask, "--ask", &mut s.rules.ask),
    ] {
        for r in list {
            out.push(
                Rule::parse(r)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| flag.to_owned())?,
            );
        }
    }
    s.plan_review |= args.plan_review;
    let explicit = match (args.mode, container_sandbox) {
        (Some(m), true) if m != PermissionMode::Bypass => bail!(
            "--container-sandbox selects the Bypass permission mode; it cannot be combined with \
             --permission-mode {}",
            m.as_str().replace('_', "-")
        ),
        (_, true) => Some(PermissionMode::Bypass),
        (m, false) => m,
    };
    let source = match explicit {
        Some(m) => {
            s.mode = m;
            ModeSource::CommandLine
        }
        // `--allow` alone (or `--may-grant`, its older name) asks for
        // programs to be granted: that is Auto's allow set, so a run given
        // one and no mode runs in Auto rather than asking for what it allowed.
        None if (!args.allow.is_empty() || !args.allow_paths.is_empty())
            && s.mode == PermissionMode::Manual =>
        {
            s.mode = PermissionMode::Auto;
            ModeSource::CommandLine
        }
        None => ModeSource::UserConfig,
    };
    Ok((s, source))
}

/// Build the live control for a command in `root` from the user's file and
/// the flags. Bypass is refused when the user disabled it or as root outside
/// a container that says so, and announced on stderr when on.
pub fn control(root: &Path, args: &PermissionArgs) -> Result<Arc<PermissionControl>> {
    let container = container_sandbox(
        args.container_sandbox,
        std::env::var(CONTAINER_SANDBOX_ENV).ok().as_deref(),
    )?;
    let file = match user_file(args.permissions_file.as_deref()) {
        Some(path) => PermissionSettings::load(&path, Some(root)).with_context(|| {
            format!("cannot read the permission settings at {}", path.display())
        })?,
        None => PermissionSettings::default(),
    };
    let (settings, source) = settings(file, args, container)?;
    let control = PermissionControl::new(settings, source, ProcessFacts::detect())
        .context("the permission mode")?;
    if control.mode() == PermissionMode::Bypass {
        eprintln!("{BYPASS_NOTICE}");
    }
    Ok(Arc::new(control))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_sandbox_needs_the_flag_and_the_variable() {
        assert!(container_sandbox(true, Some("1")).expect("on"));
        assert!(!container_sandbox(false, Some("1")).expect("off"));
        assert!(!container_sandbox(false, None).expect("off"));
        for env in [None, Some("0"), Some("yes")] {
            let err = container_sandbox(true, env).expect_err("refused");
            assert!(err.to_string().contains(CONTAINER_SANDBOX_ENV), "{err}");
        }
    }

    #[test]
    fn flags_set_the_mode_and_add_to_the_file() {
        let file = PermissionSettings::from_toml_str(
            "mode = \"accept_edits\"\ndeny = [\"shell(curl*)\"]\n[allow]\nprograms = [\"node\"]",
        )
        .expect("file");
        let (s, source) = settings(file.clone(), &PermissionArgs::default(), false).expect("s");
        assert_eq!(
            (s.mode, source),
            (PermissionMode::AcceptEdits, ModeSource::UserConfig)
        );

        let args = PermissionArgs {
            mode: Some(PermissionMode::Bypass),
            allow: vec!["python3".into()],
            ask: vec!["shell(git push*)".into()],
            plan_review: true,
            ..PermissionArgs::default()
        };
        let (s, source) = settings(file, &args, false).expect("s");
        assert_eq!(
            (s.mode, source),
            (PermissionMode::Bypass, ModeSource::CommandLine)
        );
        assert!(s.allow.allows_program("node") && s.allow.allows_program("python3"));
        assert_eq!((s.rules.deny.len(), s.rules.ask.len()), (1, 1));
        assert!(s.plan_review);
    }

    /// `--may-grant python3` (now `--allow`) alone runs in Auto: what it
    /// allowed is granted, not asked about.
    #[test]
    fn an_allow_flag_alone_selects_auto_and_the_container_flag_selects_bypass() {
        let args = PermissionArgs {
            allow: vec!["python3".into()],
            ..PermissionArgs::default()
        };
        let (s, _) = settings(PermissionSettings::default(), &args, false).expect("s");
        assert_eq!(s.mode, PermissionMode::Auto);
        let (s, source) = settings(
            PermissionSettings::default(),
            &PermissionArgs::default(),
            true,
        )
        .expect("s");
        assert_eq!(
            (s.mode, source),
            (PermissionMode::Bypass, ModeSource::CommandLine)
        );
        let clash = PermissionArgs {
            mode: Some(PermissionMode::Manual),
            ..PermissionArgs::default()
        };
        assert!(settings(PermissionSettings::default(), &clash, true).is_err());
        let bad = PermissionArgs {
            deny: vec!["shell(".into()],
            ..PermissionArgs::default()
        };
        assert!(settings(PermissionSettings::default(), &bad, false).is_err());
    }

    /// A permissions file inside the project is refused, so a worker that
    /// can write the project cannot choose the mode.
    #[test]
    fn a_permissions_file_inside_the_project_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let inside = dir.path().join("perm.toml");
        std::fs::write(&inside, "mode = \"bypass\"").expect("write");
        let err = control(
            dir.path(),
            &PermissionArgs {
                permissions_file: Some(inside),
                ..PermissionArgs::default()
            },
        )
        .expect_err("refused");
        assert!(format!("{err:#}").contains("inside the project"), "{err:#}");
    }

    /// A disabled Bypass is refused from the command line too.
    #[test]
    fn a_disabled_bypass_is_refused_from_the_command_line() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        std::fs::create_dir_all(&project).expect("mkdir");
        let file = dir.path().join("user.toml");
        std::fs::write(&file, "disable_bypass = true").expect("write");
        let err = control(
            &project,
            &PermissionArgs {
                mode: Some(PermissionMode::Bypass),
                permissions_file: Some(file),
                ..PermissionArgs::default()
            },
        )
        .expect_err("refused");
        assert!(format!("{err:#}").contains("disable_bypass"), "{err:#}");
    }
}
