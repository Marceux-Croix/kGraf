//! This crate is `mcp-echo-server`'s binary (`src/main.rs`); it exists only to be
//! spawned as a real second process by `kgraf-mcp`'s tests and benches
//! (verified against a real server), never to be linked as a library. This trivial lib target exists only
//! because Cargo resolves `[dependencies]`/`[dev-dependencies]` entries as library
//! crates: a bin-only package cannot be depended on at all, and `kgraf-mcp`
//! depending on this crate is exactly how Cargo's `CARGO_BIN_EXE_mcp-echo-server`
//! mechanism reaches its tests and benches.
