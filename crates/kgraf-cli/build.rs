//! Embeds a Windows icon and version-info resource into `kgraf.exe` (the CLI) when
//! this crate is built for `x86_64-pc-windows-msvc` (`scripts/package-windows.sh`).
//! Same mechanism as `crates/kgraf-shell/build.rs` for `kgraf-app.exe`; see that
//! file's header for the full rationale (unconditional `embed-resource`
//! build-dependency, `TARGET`-based no-op off Windows, `.manifest_required()` for
//! fail-loud on a real Windows target).
//!
//! The icon is `kgraf-shell`'s designed asset (`assets/app-icon/icon-*.png`), read by
//! relative path rather than duplicated: the CLI has no icon asset of its own, and a
//! second copy would drift from the shell's the first time either one changed.

#[path = "../../packaging/windows/ico.rs"]
mod ico;
#[path = "../../packaging/windows/manifest.rs"]
mod manifest;
#[path = "../../packaging/windows/version_rc.rs"]
mod version_rc;

/// Sizes embedded in the `.ico`; see `kgraf-shell/build.rs` for why these six.
const ICON_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

/// Returns the asset PNG bytes for one of `ICON_SIZES`, unchanged.
fn asset_png(size: u32) -> &'static [u8] {
    match size {
        16 => include_bytes!("../kgraf-shell/assets/app-icon/icon-16.png"),
        32 => include_bytes!("../kgraf-shell/assets/app-icon/icon-32.png"),
        48 => include_bytes!("../kgraf-shell/assets/app-icon/icon-48.png"),
        64 => include_bytes!("../kgraf-shell/assets/app-icon/icon-64.png"),
        128 => include_bytes!("../kgraf-shell/assets/app-icon/icon-128.png"),
        256 => include_bytes!("../kgraf-shell/assets/app-icon/icon-256.png"),
        other => panic!("no embedded icon asset for size {other}"),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for size in ICON_SIZES {
        println!("cargo:rerun-if-changed=../kgraf-shell/assets/app-icon/icon-{size}.png");
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
    let ico_path = format!("{out_dir}/kgraf.ico");
    std::fs::write(&ico_path, &ico_bytes)
        .unwrap_or_else(|e| panic!("writing generated icon to {ico_path}: {e}"));

    let manifest_path = format!("{out_dir}/kgraf.exe.manifest");
    std::fs::write(&manifest_path, manifest::MANIFEST_XML)
        .unwrap_or_else(|e| panic!("writing generated manifest to {manifest_path}: {e}"));

    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by cargo");
    let rc_path = format!("{out_dir}/kgraf.rc");
    let rc = version_rc::windows_resource_rc(
        "kgraf",
        "kgraf.exe",
        "kGraf command-line interface",
        &version,
        &ico_path,
        &manifest_path,
    );
    std::fs::write(&rc_path, rc)
        .unwrap_or_else(|e| panic!("writing generated resource script to {rc_path}: {e}"));

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_required()
        .unwrap_or_else(|e| {
            panic!("embedding the Windows icon/version resource into kgraf.exe: {e}")
        });
}
