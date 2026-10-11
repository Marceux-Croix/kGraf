//! kGraf's Windows self-test runner.
//!
//! One program, shipped in a zip with kGraf's cross-built Windows test
//! binaries: it records the machine, runs the tests and real `kgraf.exe`
//! checks without Rust on the machine, writes one report to the Desktop,
//! and removes everything else it made. See `README.txt` in the bundle
//! (`crates/kgraf-wintest/bundle/`) and docs/WINDOWS-TESTING.md.
//!
//! It is a workspace crate, not a standalone experiment, because it links
//! kGraf's own sandbox and secret scrubber, and its parsers and cleanup need
//! the workspace's tests, clippy and formatting gates.

pub mod budget;
pub mod cli;
pub mod console;
pub mod e2e;
pub mod facts;
pub mod libtest;
pub mod manifest;
pub mod only;
pub mod pack;
pub mod platform;
pub mod proc;
pub mod progress;
pub mod redact;
pub mod report;
pub mod runner;
pub mod sandbox_access;
pub mod sandbox_phase;
pub mod sandbox_run;
pub mod stub;
pub mod workdir;
