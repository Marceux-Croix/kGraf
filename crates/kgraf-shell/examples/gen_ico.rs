//! Standalone `.ico` generator for `scripts/package-windows.sh`'s MSI step, which needs
//! an on-disk icon file to point `wixl`'s `<Icon>` element (`ARPPRODUCTICON`, the
//! Add/Remove Programs entry) at -- unlike the exe-embedded icon in `build.rs`, which
//! writes straight into `OUT_DIR` and never needs a script-visible path.
//!
//! Reads the same designed asset PNGs `build.rs` embeds (`assets/app-icon/icon-*.png`)
//! and packs their bytes into the `.ico` unchanged (`packaging/windows/ico.rs`'s
//! PNG-in-ICO entries), so the Add/Remove Programs icon and the .exe's own icon are
//! byte-identical.
//!
//! Run via `cargo run --release --example gen_ico -p kgraf-shell -- <output.ico>`.

#[path = "../../../packaging/windows/ico.rs"]
mod ico;

const SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

fn asset_path(size: u32) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/app-icon")
        .join(format!("icon-{size}.png"))
}

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .expect("usage: gen_ico <output.ico>");

    let pngs: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let path = asset_path(size);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("reading icon asset {}: {e}", path.display()));
            (size, bytes)
        })
        .collect();
    let ico_bytes = ico::encode_ico(&pngs);
    std::fs::write(&out_path, &ico_bytes).unwrap_or_else(|e| panic!("writing {out_path}: {e}"));
}
