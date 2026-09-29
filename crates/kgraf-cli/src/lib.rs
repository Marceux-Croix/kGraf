//! The library half of the `kgraf` command: the parts the binary shares with
//! its tests. Everything user-facing (argument parsing, printing) stays in the
//! binary.
//!
//! `worker` lives in `kgraf-host` now (the orchestrator's attempt environments
//! are assembled through it for every host: this command and the desktop shell);
//! it is re-exported here so `kgraf_cli::worker::...` paths are unchanged.

pub mod logging;
pub use kgraf_host::worker;
