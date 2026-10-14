//! `kgraf orchestrate`'s between-run edit flags (decision 0100, sections 5
//! and 7): `--change` and `--edit` reopen a run with one delta call against
//! its plan; `--from-run` starts a new run from a copy of an earlier run's
//! plan. With none of them, `--resume` and `--instruction` behave as before.

use std::path::{Path, PathBuf};

/// The flags, flattened into `kgraf orchestrate`.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct EditFlags {
    /// With `--resume RUN`: a text file holding a change to the run. A
    /// completed or stopped run reopens; the change costs one re-plan call
    /// against its plan, with no new decomposition, and runs only what it
    /// adds or changes on the run's branch. The file is your yes for a
    /// change that adds to or reshapes the plan; one that cuts running or
    /// verified work follows your permission mode.
    #[arg(
        long,
        value_name = "FILE",
        requires = "resume",
        conflicts_with_all = ["edit", "from_run"]
    )]
    pub change: Option<PathBuf>,
    /// With `--resume RUN`: a text file holding the run's instruction as you
    /// edited it. The lines that differ are the change, sent with the whole
    /// edited text in one re-plan call. An edit that changes no line sends
    /// nothing.
    #[arg(
        long,
        value_name = "FILE",
        requires = "resume",
        conflicts_with = "from_run"
    )]
    pub edit: Option<PathBuf>,
    /// With `--instruction FILE`: start a new run from a copy of RUN's plan.
    /// Its contracts whose verified work is already in the new run's base are
    /// done; the rest remain, and one re-plan call applies the difference
    /// between RUN's instruction and FILE.
    #[arg(
        long,
        value_name = "RUN",
        requires = "instruction",
        conflicts_with = "resume"
    )]
    pub from_run: Option<String>,
}

/// What a between-run edit asks for, its files read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Change(String),
    Edited(String),
    Copy { source: String },
}

impl EditFlags {
    /// The edit these flags ask for, with its file read. `None`: no edit
    /// flag was given.
    pub fn read(&self) -> anyhow::Result<Option<Edit>> {
        Ok(match (&self.change, &self.edit, &self.from_run) {
            (Some(path), _, _) => Some(Edit::Change(read_text(path)?)),
            (None, Some(path), _) => Some(Edit::Edited(read_text(path)?)),
            (None, None, Some(source)) => Some(Edit::Copy {
                source: source.clone(),
            }),
            (None, None, None) => None,
        })
    }
}

fn read_text(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    if text.trim().is_empty() {
        anyhow::bail!("{} is empty: there is no change in it", path.display());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[arg(long)]
        resume: Option<String>,
        #[arg(long)]
        instruction: Option<PathBuf>,
        #[command(flatten)]
        flags: EditFlags,
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut argv = vec!["kgraf"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
    }

    /// Each flag needs the flag it edits, and the three exclude each other.
    #[test]
    fn each_flag_needs_its_partner_and_they_exclude_each_other() {
        let none = parse(&["--resume", "r1"]).expect("plain resume");
        assert_eq!(none.flags, EditFlags::default());
        assert!(parse(&["--resume", "r1", "--change", "c.md"]).is_ok());
        assert!(parse(&["--resume", "r1", "--edit", "e.md"]).is_ok());
        assert!(parse(&["--instruction", "i.md", "--from-run", "r0"]).is_ok());
        assert!(
            parse(&["--change", "c.md"]).is_err(),
            "--change needs --resume"
        );
        assert!(parse(&["--edit", "e.md"]).is_err(), "--edit needs --resume");
        assert!(
            parse(&["--from-run", "r0"]).is_err(),
            "--from-run needs --instruction"
        );
        assert!(parse(&["--resume", "r1", "--change", "c.md", "--edit", "e.md"]).is_err());
        assert!(parse(&["--resume", "r1", "--from-run", "r0"]).is_err());
    }

    /// An empty change file is refused before anything starts.
    #[test]
    fn an_empty_change_file_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("c.md");
        std::fs::write(&path, "  \n").expect("write");
        let flags = EditFlags {
            change: Some(path.clone()),
            ..Default::default()
        };
        assert!(flags.read().is_err());
        std::fs::write(&path, "add a test").expect("write");
        assert_eq!(
            flags.read().expect("read"),
            Some(Edit::Change("add a test".into()))
        );
    }
}
