//! Embeds a Windows icon and version-info resource into `kgraf-app.exe` when this
//! crate is built for `x86_64-pc-windows-msvc` (`scripts/package-windows.sh`).
//!
//! `embed-resource` (MIT, `[build-dependencies]` in `Cargo.toml`) is unconditional
//! there, not gated with `target.'cfg(windows)'`: Cargo evaluates
//! `target.'cfg(...)'.build-dependencies` against the HOST running the build script,
//! not the crate's target, so a `cfg(windows)` gate there would only fire when building
//! ON Windows -- never during this project's actual path, cross-compiling for Windows
//! FROM macOS with cargo-xwin. `embed_resource::compile()` itself is the one that reads
//! Cargo's `TARGET` build-script env var (always the real target, host or not) and picks
//! `llvm-rc` for a `-windows-msvc` target; it is a safe no-op (`NotWindows`) on every
//! other target, so this file runs unconditionally and costs one tiny icon/rc write on
//! every build, including the native macOS dev build, clippy and `cargo test`.
//!
//! The icon is `assets/app-icon/icon-*.png`, the same designed asset the window and
//! tray icons decode at runtime (`src/icon.rs`). A Windows `.ico` may carry a complete
//! PNG file per entry (Vista and later read this directly), so this file embeds the
//! asset bytes unchanged: no decoding, no re-encoding, and the `.ico` is pixel-identical
//! to what ships everywhere else. `packaging/windows/ico.rs`, `packaging/windows/
//! manifest.rs` and `packaging/windows/version_rc.rs` are shared with `kgraf-cli/
//! build.rs` via `#[path]` (reuse this project's own code rather than duplicate it);
//! each is independently unit-tested by compiling it standalone. build.rs itself is
//! not, since it links `embed-resource` and cannot compile alone.
//!
//! `.manifest_required()` rather than `.manifest_optional()`: fail loud -- on an
//! actual Windows target, a missing `llvm-rc` must abort the build with a clear error,
//! not silently ship an unresourced .exe. Off-Windows targets still return `Ok` (as
//! `NotWindows`), regardless of which method is called.

#[path = "../../packaging/windows/ico.rs"]
mod ico;
#[path = "../../packaging/windows/manifest.rs"]
mod manifest;
#[path = "../../packaging/windows/version_rc.rs"]
mod version_rc;

/// Sizes embedded in the `.ico`: the standard small/medium/large/high-DPI set Windows
/// Explorer, the taskbar and Alt-Tab pick between depending on display scaling.
const ICON_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

/// Returns the asset PNG bytes for one of `ICON_SIZES`, unchanged.
fn asset_png(size: u32) -> &'static [u8] {
    match size {
        16 => include_bytes!("assets/app-icon/icon-16.png"),
        32 => include_bytes!("assets/app-icon/icon-32.png"),
        48 => include_bytes!("assets/app-icon/icon-48.png"),
        64 => include_bytes!("assets/app-icon/icon-64.png"),
        128 => include_bytes!("assets/app-icon/icon-128.png"),
        256 => include_bytes!("assets/app-icon/icon-256.png"),
        other => panic!("no embedded icon asset for size {other}"),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for size in ICON_SIZES {
        println!("cargo:rerun-if-changed=assets/app-icon/icon-{size}.png");
    }
    println!("cargo:rerun-if-changed=../../packaging/windows/ico.rs");
    println!("cargo:rerun-if-changed=../../packaging/windows/version_rc.rs");
    println!("cargo:rerun-if-changed=../../packaging/windows/manifest.rs");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo for every build script");

    let pngs: Vec<(u32, Vec<u8>)> = ICON_SIZES
        .iter()
        .map(|&size| (size, asset_png(size).to_vec()))
        .collect();
    let ico_bytes = ico::encode_ico(&pngs);
    let ico_path = format!("{out_dir}/kgraf-app.ico");
    std::fs::write(&ico_path, &ico_bytes)
        .unwrap_or_else(|e| panic!("writing generated icon to {ico_path}: {e}"));

    let manifest_path = format!("{out_dir}/kgraf-app.exe.manifest");
    std::fs::write(&manifest_path, manifest::MANIFEST_XML)
        .unwrap_or_else(|e| panic!("writing generated manifest to {manifest_path}: {e}"));

    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by cargo");
    let rc_path = format!("{out_dir}/kgraf-app.rc");
    let rc = version_rc::windows_resource_rc(
        "kgraf-app",
        "kgraf-app.exe",
        "kGraf desktop shell",
        &version,
        &ico_path,
        &manifest_path,
    );
    std::fs::write(&rc_path, rc)
        .unwrap_or_else(|e| panic!("writing generated resource script to {rc_path}: {e}"));

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_required()
        .unwrap_or_else(|e| {
            panic!("embedding the Windows icon/version resource into kgraf-app.exe: {e}")
        });
}
