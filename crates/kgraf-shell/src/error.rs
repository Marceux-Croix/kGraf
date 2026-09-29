//! Library errors. The binary wraps these in `anyhow` at its boundary.

use crate::adapter::AdapterPolicy;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    /// No adapter satisfies the policy. Raised instead of letting the toolkit pick a
    /// default or panic deep in surface creation.
    #[error(
        "no usable GPU or software adapter under policy {policy:?}; adapters seen: {seen}. \
         {hint}"
    )]
    NoAdapter {
        policy: AdapterPolicy,
        seen: String,
        hint: &'static str,
    },

    #[error(
        "compaction point {percent}% is outside the allowed range {min}% to {max}%",
        min = crate::settings::COMPACTION_MIN_PERCENT,
        max = crate::settings::COMPACTION_MAX_PERCENT
    )]
    CompactionOutOfRange { percent: i64 },

    #[error("{what} {value} is outside the allowed range 1 to {max}")]
    CapOutOfRange {
        what: &'static str,
        value: i64,
        max: usize,
    },

    #[error("invalid settings file: {0}")]
    Config(String),

    #[error("invalid icon image: {0}")]
    Icon(String),

    #[error("tray icon could not be created: {0}")]
    Tray(String),

    #[error("the window or renderer failed to start: {0}")]
    Window(String),

    /// The project's state could not be moved from its former name
    /// (`.haustoria`, decision 0062), or both names exist.
    #[error("the project's state directory: {0:#}")]
    StateMigration(#[source] kgraf_host::Error),

    #[error("settings file {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

impl kgraf_diag::DiagCode for ShellError {
    fn diag_code(&self) -> &'static str {
        match self {
            ShellError::NoAdapter { .. } => "KG-0901",
            ShellError::Config(_) => "KG-0902",
            _ => kgraf_diag::catch_all_for("kgraf-shell").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-shell"
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch resolves to a real glossary entry. One instance per named branch,
    /// plus one unnamed variant for the catch-all.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let instances: Vec<ShellError> = vec![
            ShellError::NoAdapter {
                policy: AdapterPolicy::SoftwareOnly,
                seen: "x".into(),
                hint: "x",
            },
            ShellError::Config("x".into()),
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            ShellError::Icon("x".into()),
        ];
        for e in &instances {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}
