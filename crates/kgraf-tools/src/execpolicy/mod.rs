//! Command policy: allow, prompt or forbid a command by argv prefix.
//!
//! The prefix-matching core ([`rule::PrefixPattern`], [`rule::PatternToken`]) is
//! ported from codex-rs `execpolicy` (0055, `third_party/codex/UPSTREAM`).
//! [`policy::ExecPolicy`] and [`decision::Decision`]'s `read_only` extension are
//! kGraf's own; see `policy`'s doc comment for what upstream provides that
//! this module deliberately does not take (the Starlark policy-authoring
//! language). The build and test allowlist ([`BuildAllowlist`], decision
//! 0084) is kGraf's own as well, as is the refusal of package downloads
//! outside a disposable remote environment (`package_fetch`, decision 0087).

mod build;
mod build_tools;
mod decision;
mod package_fetch;
mod policy;
mod rule;

pub use build::split_command_line;
pub use build::{
    AllowlistMatch, BuildAllowlist, BuildCommand, CargoSubcommand, ToolCommand, UnknownBuildCommand,
};
pub(crate) use build::{cargo_rule_subject_allowed, has_shell_syntax, manifest_paths_of};
pub use decision::Decision;
pub use package_fetch::{PACKAGE_FETCH_MARK, is_package_fetch_refusal};
pub(crate) use policy::worktree_cd;
pub use policy::{CommandRule, ExecPolicy, PolicyVerdict};
pub use rule::{PatternToken, PrefixPattern};

#[cfg(test)]
mod script_build_tests;
