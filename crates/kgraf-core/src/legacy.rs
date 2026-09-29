//! kGraf's former name, Haustoria (decision 0062), and the one place that
//! knows it.
//!
//! The product was renamed on 2026-09-27. What a user of the former name
//! already has on disk or in the environment keeps working:
//!
//! - **State directory.** A project's state lives in [`STATE_DIR`]. A project
//!   that still has [`LEGACY_STATE_DIR`] is moved on first use (the move itself
//!   is `kgraf-host`'s `migrate` module, which also repairs what points into
//!   the old path).
//! - **Environment variables.** Every `KGRAF_*` variable kGraf reads is read
//!   through [`env_var`]: when only the former `HAUSTORIA_*` spelling is set,
//!   its value is used and a warning naming the new variable is printed once
//!   per process.
//!
//! Nothing else in the workspace spells the former name except the migration
//! code, its tests, and the compatibility names defined here.

use std::collections::BTreeSet;
use std::sync::Mutex;

/// The project state directory, relative to the project root.
pub const STATE_DIR: &str = ".kgraf";

/// The state directory under the former name. A project that has it and not
/// [`STATE_DIR`] is migrated on first use; one that has both is refused.
pub const LEGACY_STATE_DIR: &str = ".haustoria";

/// The prefix of every environment variable kGraf reads.
pub const ENV_PREFIX: &str = "KGRAF_";

/// The prefix the same variables had under the former name.
pub const LEGACY_ENV_PREFIX: &str = "HAUSTORIA_";

/// What [`resolve_env`] found for one variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvLookup {
    /// The value to use, if either spelling is set.
    pub value: Option<String>,
    /// A warning the caller must show once: the former spelling was used, or
    /// was set and ignored because the new one is set too.
    pub warning: Option<String>,
}

/// The former spelling of `name`, when `name` is a `KGRAF_*` variable.
pub fn legacy_env_name(name: &str) -> Option<String> {
    name.strip_prefix(ENV_PREFIX)
        .map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
}

/// Resolve `name` through `lookup` (which is `std::env::var` in production),
/// falling back to its former `HAUSTORIA_*` spelling. Pure, so the rule is
/// tested without touching the process environment.
///
/// - `KGRAF_X` set: its value, and a warning only if `HAUSTORIA_X` is set too
///   (an ignored setting is never silent).
/// - only `HAUSTORIA_X` set: its value, and a warning naming `KGRAF_X`.
/// - neither: `None`, no warning.
pub fn resolve_env(name: &str, lookup: impl Fn(&str) -> Option<String>) -> EnvLookup {
    let current = lookup(name);
    let Some(legacy) = legacy_env_name(name) else {
        return EnvLookup {
            value: current,
            warning: None,
        };
    };
    let former = lookup(&legacy);
    match (current, former) {
        (Some(value), Some(_)) => EnvLookup {
            value: Some(value),
            warning: Some(format!(
                "warning: {legacy} is ignored because {name} is set; {legacy} is the former \
                 name of {name} (Haustoria was renamed kGraf, decision 0062). Unset {legacy}."
            )),
        },
        (Some(value), None) => EnvLookup {
            value: Some(value),
            warning: None,
        },
        (None, Some(value)) => EnvLookup {
            value: Some(value),
            warning: Some(format!(
                "warning: using {legacy}; rename it to {name} (Haustoria was renamed kGraf, \
                 decision 0062)."
            )),
        },
        (None, None) => EnvLookup {
            value: None,
            warning: None,
        },
    }
}

/// Variables already warned about in this process. Process-wide on purpose:
/// the requirement is one warning per variable per process, whichever crate
/// reads it; a context object would have to be threaded through every
/// credential lookup for no other gain.
static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Returns `true` the first time it is called for `name` in this process.
fn first_warning_for(name: &str) -> bool {
    let mut warned = WARNED.lock().unwrap_or_else(|p| p.into_inner());
    warned.insert(name.to_owned())
}

/// Print `message` on stderr the first time this process asks for `key`.
/// For every other compatibility warning about the former name (a
/// `${HAUSTORIA_WORKTREE}` placeholder in a configuration, for example).
pub fn warn_once(key: &str, message: &str) {
    if first_warning_for(key) {
        eprintln!("{message}");
    }
}

/// Read environment variable `name` (a `KGRAF_*` name, or any other), falling
/// back to its former `HAUSTORIA_*` spelling with a warning on stderr, once
/// per variable per process. A value that is not valid Unicode reads as unset,
/// as `std::env::var(..).ok()` does.
pub fn env_var(name: &str) -> Option<String> {
    let found = resolve_env(name, |n| std::env::var(n).ok());
    if let Some(warning) = &found.warning
        && first_warning_for(name)
    {
        eprintln!("{warning}");
    }
    found.value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn only_the_former_name_is_used_with_a_warning_naming_the_new_one() {
        let found = resolve_env("KGRAF_LOG", env(&[("HAUSTORIA_LOG", "debug")]));
        assert_eq!(found.value.as_deref(), Some("debug"));
        let warning = found.warning.expect("a warning");
        assert!(warning.contains("HAUSTORIA_LOG"), "{warning}");
        assert!(warning.contains("rename it to KGRAF_LOG"), "{warning}");
    }

    #[test]
    fn the_new_name_wins_and_an_ignored_former_name_is_said() {
        let found = resolve_env(
            "KGRAF_LOG",
            env(&[("KGRAF_LOG", "info"), ("HAUSTORIA_LOG", "debug")]),
        );
        assert_eq!(found.value.as_deref(), Some("info"));
        assert!(
            found
                .warning
                .expect("a warning")
                .contains("HAUSTORIA_LOG is ignored")
        );
    }

    #[test]
    fn the_new_name_alone_is_silent_and_neither_is_none() {
        let found = resolve_env("KGRAF_LOG", env(&[("KGRAF_LOG", "info")]));
        assert_eq!(found.value.as_deref(), Some("info"));
        assert_eq!(found.warning, None);
        let none = resolve_env("KGRAF_LOG", env(&[]));
        assert_eq!(none.value, None);
        assert_eq!(none.warning, None);
    }

    #[test]
    fn a_name_outside_the_prefix_has_no_former_spelling() {
        let found = resolve_env("OPENAI_API_KEY", env(&[("HAUSTORIA_API_KEY", "x")]));
        assert_eq!(found.value, None);
        assert_eq!(found.warning, None);
        assert_eq!(legacy_env_name("PATH"), None);
        assert_eq!(
            legacy_env_name("KGRAF_KIMI_API_KEY").as_deref(),
            Some("HAUSTORIA_KIMI_API_KEY")
        );
    }

    #[test]
    fn each_variable_is_warned_about_once_per_process() {
        assert!(first_warning_for("KGRAF_TEST_ONCE"));
        assert!(!first_warning_for("KGRAF_TEST_ONCE"));
        assert!(first_warning_for("KGRAF_TEST_OTHER"));
    }
}
