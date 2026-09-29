//! Minimal PNG encoder for the desktop shell's `--screenshot` window capture
//! (`crates/kgraf-shell/src/app.rs`).
//!
//! `compress_to_vec_zlib` (`miniz_oxide`, already linked transitively through
//! `image`/`png` in this workspace's dependency graph -- see `Cargo.lock`) returns a
//! complete RFC 1950 zlib stream (header, DEFLATE body, Adler-32 trailer) -- exactly
//! the PNG `IDAT` payload shape, so no manual zlib/DEFLATE framing code is needed here.

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(kind);
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// Deflate compression level, 0 (none) to 10 (best, `miniz_oxide`'s own max): a
/// window screenshot is a build/debug artifact, not a hot path, so "best" costs
/// nothing noticeable.
const COMPRESSION_LEVEL: u8 = 8;

/// Encodes a `width` by `height` RGBA8 buffer as a PNG (8-bit depth, color type 6 =
/// RGBA, filter 0 per row); the desktop shell's `--screenshot` uses it for window
/// captures.
pub fn encode_png_rect(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    debug_assert_eq!(rgba.len(), w * h * 4);

    let mut raw = Vec::with_capacity(h * (1 + w * 4));
    for row in 0..h {
        raw.push(0); // filter type 0: None
        let start = row * w * 4;
        raw.extend_from_slice(&rgba[start..start + w * 4]);
    }
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, COMPRESSION_LEVEL);

    let mut png = Vec::new();
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type: RGBA
    ihdr.push(0); // compression method
    ihdr.push(0); // filter method
    ihdr.push(0); // interlace method
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &compressed);
    write_chunk(&mut png, b"IEND", &[]);
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_the_png_signature() {
        let rgba = vec![0u8; 4 * 2 * 2];
        let png = encode_png_rect(2, 2, &rgba);
        assert_eq!(&png[0..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    #[test]
    fn ends_with_an_iend_chunk() {
        let rgba = vec![0u8; 4 * 2 * 2];
        let png = encode_png_rect(2, 2, &rgba);
        // IEND: 4-byte zero length, "IEND", then its CRC -- always the last 12 bytes.
        let tail = &png[png.len() - 12..];
        assert_eq!(&tail[0..4], &[0, 0, 0, 0]);
        assert_eq!(&tail[4..8], b"IEND");
    }

    #[test]
    fn round_trips_through_a_real_decompressor() {
        // The IDAT payload is real DEFLATE output: decode it back with miniz_oxide's
        // own inflate and check it reproduces the exact filtered scanlines
        // encode_png_rect built, not just a structural PNG-framing invariant.
        let rgba: Vec<u8> = (0u8..=255).cycle().take(4 * 3 * 3).collect();
        let png = encode_png_rect(3, 3, &rgba);

        // Extract the IDAT chunk's data (length-prefixed, type, data, then a 4-byte
        // CRC) rather than depending on a PNG-parsing crate.
        let mut pos = 8; // past the signature
        let mut idat: Option<&[u8]> = None;
        while pos + 8 <= png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data_start = pos + 8;
            if kind == b"IDAT" {
                idat = Some(&png[data_start..data_start + len]);
                break;
            }
            pos = data_start + len + 4; // + CRC
        }
        let idat = idat.expect("PNG must contain an IDAT chunk");

        let decompressed =
            miniz_oxide::inflate::decompress_to_vec_zlib(idat).expect("valid zlib stream");
        let n = 3usize;
        let mut expected = Vec::with_capacity(n * (1 + n * 4));
        for row in 0..n {
            expected.push(0);
            let start = row * n * 4;
            expected.extend_from_slice(&rgba[start..start + n * 4]);
        }
        assert_eq!(decompressed, expected);
    }
}
