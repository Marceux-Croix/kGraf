//! The application icon: the designed PNG assets under `assets/app-icon/`, embedded at
//! compile time and decoded to RGBA at runtime.
//!
//! `assets/app-icon/icon-{16,32,48,64,128,256,512,1024}.png` are the single source for
//! every place this app shows its icon (window, tray, and the packaging scripts under
//! `packaging/` and `scripts/`, which copy or embed the same files). This module never
//! draws or resamples: it decodes one of the embedded sizes exactly, picking whichever
//! is closest to what the caller asked for.
//!
//! `png` (already in this workspace's lockfile at 0.18.1, pulled in transitively through
//! `tray-icon`; pinned to the same major here so no second copy is resolved) does the
//! decoding. A missing or malformed embedded asset panics rather than falling back to a
//! blank icon: a build that ships a broken icon asset is a defect, not something to hide.

use std::io::Cursor;

/// Square RGBA8 image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    pub size: u32,
    pub rgba: Vec<u8>,
}

struct Asset {
    size: u32,
    bytes: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        size: 16,
        bytes: include_bytes!("../assets/app-icon/icon-16.png"),
    },
    Asset {
        size: 32,
        bytes: include_bytes!("../assets/app-icon/icon-32.png"),
    },
    Asset {
        size: 48,
        bytes: include_bytes!("../assets/app-icon/icon-48.png"),
    },
    Asset {
        size: 64,
        bytes: include_bytes!("../assets/app-icon/icon-64.png"),
    },
    Asset {
        size: 128,
        bytes: include_bytes!("../assets/app-icon/icon-128.png"),
    },
    Asset {
        size: 256,
        bytes: include_bytes!("../assets/app-icon/icon-256.png"),
    },
    Asset {
        size: 512,
        bytes: include_bytes!("../assets/app-icon/icon-512.png"),
    },
    Asset {
        size: 1024,
        bytes: include_bytes!("../assets/app-icon/icon-1024.png"),
    },
];

/// Decodes the embedded icon asset closest to `size` pixels square.
///
/// No runtime resampling: this returns whichever embedded PNG's own size is nearest to
/// `size`, decoded at that native resolution.
pub fn asset(size: u32) -> IconImage {
    let chosen = ASSETS
        .iter()
        .min_by_key(|a| a.size.abs_diff(size))
        .expect("ASSETS is never empty");
    decode(chosen)
}

fn decode(asset: &Asset) -> IconImage {
    let decoder = png::Decoder::new(Cursor::new(asset.bytes));
    let mut reader = decoder.read_info().unwrap_or_else(|e| {
        panic!(
            "embedded icon asset for size {} is not a PNG: {e}",
            asset.size
        )
    });
    let (color_type, bit_depth) = reader.output_color_type();
    assert_eq!(
        color_type,
        png::ColorType::Rgba,
        "embedded icon asset for size {} must be RGBA, found {color_type:?}",
        asset.size
    );
    assert_eq!(
        bit_depth,
        png::BitDepth::Eight,
        "embedded icon asset for size {} must be 8-bit, found {bit_depth:?}",
        asset.size
    );
    let mut buf = vec![
        0;
        reader.output_buffer_size().unwrap_or_else(|| panic!(
            "embedded icon asset for size {} has no computable buffer size",
            asset.size
        ))
    ];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("decoding embedded icon asset for size {}: {e}", asset.size));
    assert_eq!(
        info.width, info.height,
        "embedded icon asset for size {} is not square: {}x{}",
        asset.size, info.width, info.height
    );
    assert_eq!(
        info.width, asset.size,
        "embedded icon asset named size {} actually decodes to {}",
        asset.size, info.width
    );
    buf.truncate(info.buffer_size());
    IconImage {
        size: info.width,
        rgba: buf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SIZES: [u32; 8] = [16, 32, 48, 64, 128, 256, 512, 1024];

    #[test]
    fn every_embedded_size_decodes_to_its_own_square_rgba_buffer() {
        for size in ALL_SIZES {
            let img = asset(size);
            assert_eq!(img.size, size, "asset({size}) returned the wrong size");
            assert_eq!(
                img.rgba.len(),
                (size * size * 4) as usize,
                "asset({size}) has the wrong buffer length for RGBA8"
            );
        }
    }

    #[test]
    fn corners_transparent_center_opaque() {
        let img = asset(256);
        assert_eq!(img.rgba[3], 0, "top-left corner is transparent");
        let c = (img.size / 2) as usize;
        let mid = (c * img.size as usize + c) * 4;
        assert_eq!(img.rgba[mid + 3], 255, "center is opaque");
    }

    #[test]
    fn nearest_embedded_size_is_chosen_for_an_unlisted_request() {
        let img = asset(300);
        assert_eq!(img.size, 256, "300 is closer to 256 than to 512");
    }
}
