//! Minimal Windows `.ico` container writer, used only by `build.rs` (both
//! `crates/kgraf-shell/build.rs` and `crates/kgraf-cli/build.rs`) to embed an
//! icon resource when cross-compiling for `x86_64-pc-windows-msvc` with cargo-xwin
//! (`scripts/package-windows.sh`).
//!
//! Pure and dependency-free: each icon size is stored as a full PNG image inside the
//! ICO directory, taken unchanged from `assets/app-icon/`. Windows Vista and later read
//! PNG-in-ICO entries directly (the format used by every icon above 48x48, since raw
//! BMP DIB entries max out at 256 colors' worth of header complexity for large sizes);
//! nothing here needs to decode or encode an image at all. `icotool`/most Windows
//! tooling and Explorer both accept this.
//!
//! Reference: Microsoft's `ICONDIR`/`ICONDIRENTRY` layout
//! (<https://learn.microsoft.com/en-us/previous-versions/ms997538(v=msdn.10)>), the
//! `RT_ICON`/`RT_GROUP_ICON` pairing embed-resource's `ICON` RC statement builds from a
//! single `.ico` file.

/// Builds a complete `.ico` file from a list of `(size, png_bytes)` pairs, each `size`
/// being both the width and height of a square icon (ICO does not support non-square
/// entries here, and the app icon asset this project ships is square at every size).
///
/// `size` must be in `1..=256`; 256 is stored as the ICO spec's `0` sentinel (a single
/// byte cannot hold 256, so the format reserves 0 to mean "256").
pub fn encode_ico(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    assert!(!entries.is_empty(), "an .ico file needs at least one image");
    for (size, _) in entries {
        assert!(
            (1..=256).contains(size),
            "icon size {size} out of ICO's representable 1..=256 range"
        );
    }

    let count = entries.len();
    let header_len = 6 + 16 * count; // ICONDIR (6 bytes) + one ICONDIRENTRY (16 bytes) each
    let mut out =
        Vec::with_capacity(header_len + entries.iter().map(|(_, p)| p.len()).sum::<usize>());

    // ICONDIR: reserved(u16)=0, type(u16)=1 (icon, not cursor), count(u16)
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(count as u16).to_le_bytes());

    let mut offset = header_len as u32;
    for (size, png) in entries {
        let dim_byte = if *size == 256 { 0u8 } else { *size as u8 };
        out.push(dim_byte); // width
        out.push(dim_byte); // height
        out.push(0); // color count: 0 = not a palette image
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel (RGBA8)
        out.extend_from_slice(&(png.len() as u32).to_le_bytes()); // bytes in this image
        out.extend_from_slice(&offset.to_le_bytes()); // offset from file start
        offset += png.len() as u32;
    }

    for (_, png) in entries {
        out.extend_from_slice(png);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_png(tag: u8, len: usize) -> Vec<u8> {
        vec![tag; len]
    }

    #[test]
    fn header_reports_icon_type_and_entry_count() {
        let ico = encode_ico(&[(16, fake_png(1, 10)), (32, fake_png(2, 20))]);
        assert_eq!(&ico[0..2], &[0, 0], "reserved field");
        assert_eq!(&ico[2..4], &[1, 0], "type: 1 = icon");
        assert_eq!(&ico[4..6], &[2, 0], "two entries");
    }

    #[test]
    fn dimension_256_is_stored_as_the_zero_sentinel() {
        let ico = encode_ico(&[(256, fake_png(9, 5))]);
        let entry = &ico[6..6 + 16];
        assert_eq!(entry[0], 0, "width byte: 256 wraps to the 0 sentinel");
        assert_eq!(entry[1], 0, "height byte: 256 wraps to the 0 sentinel");
    }

    #[test]
    fn small_dimension_is_stored_directly() {
        let ico = encode_ico(&[(48, fake_png(9, 5))]);
        let entry = &ico[6..6 + 16];
        assert_eq!(entry[0], 48);
        assert_eq!(entry[1], 48);
    }

    #[test]
    fn offsets_are_sequential_and_data_follows_the_directory() {
        let pngs = [(16u32, fake_png(0xAA, 7)), (32u32, fake_png(0xBB, 11))];
        let ico = encode_ico(&pngs);
        let header_len = 6 + 16 * pngs.len();

        let first_off = u32::from_le_bytes(ico[6 + 12..6 + 16].try_into().unwrap());
        let second_off = u32::from_le_bytes(ico[22 + 12..22 + 16].try_into().unwrap());
        assert_eq!(first_off as usize, header_len);
        assert_eq!(second_off as usize, header_len + 7);

        assert_eq!(&ico[first_off as usize..first_off as usize + 7], &[0xAA; 7]);
        assert_eq!(
            &ico[second_off as usize..second_off as usize + 11],
            &[0xBB; 11]
        );
    }

    #[test]
    #[should_panic(expected = "at least one image")]
    fn empty_input_panics() {
        encode_ico(&[]);
    }

    #[test]
    #[should_panic(expected = "out of ICO's representable")]
    fn zero_size_panics() {
        encode_ico(&[(0, fake_png(1, 1))]);
    }
}
