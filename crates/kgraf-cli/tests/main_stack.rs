//! The main thread has room to parse the command line. Windows reserves 1 MB for
//! an executable's main thread unless its PE header asks for more, and a debug
//! build of `kgraf` needs more than that for clap's subcommand tree, so every
//! command died with STACK_OVERFLOW before it printed anything. The workspace
//! linker flag (`/STACK:8388608` in `.cargo/config.toml`) raises the reserve.

/// The stack reserve the main thread gets on macOS and Linux, in bytes.
const WANTED: u64 = 8 * 1024 * 1024;

/// `SizeOfStackReserve` from a PE32+ image, or `None` if the bytes are not one.
fn pe_stack_reserve(image: &[u8]) -> Option<u64> {
    if image.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(image.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    // The optional header follows the 4-byte signature and 20-byte file header.
    let optional = pe + 24;
    if u16::from_le_bytes(image.get(optional..optional + 2)?.try_into().ok()?) != 0x20b {
        return None;
    }
    let at = optional + 72;
    Some(u64::from_le_bytes(image.get(at..at + 8)?.try_into().ok()?))
}

#[test]
fn the_reader_finds_the_reserve_in_a_pe_header() {
    let mut image = vec![0u8; 0x200];
    image[..2].copy_from_slice(b"MZ");
    image[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    image[0x80..0x84].copy_from_slice(b"PE\0\0");
    image[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
    image[0x98 + 72..0x98 + 80].copy_from_slice(&WANTED.to_le_bytes());
    assert_eq!(pe_stack_reserve(&image), Some(WANTED));
    assert_eq!(pe_stack_reserve(b"\x7fELF"), None);
}

/// Reads the binary this test was built with. On a Windows image the reserve
/// must be at least 8 MB; other formats have no PE header and are skipped.
#[test]
fn a_windows_binary_reserves_a_main_stack_as_large_as_unix_gives() {
    let image = std::fs::read(env!("CARGO_BIN_EXE_kgraf")).expect("read kgraf");
    if let Some(reserve) = pe_stack_reserve(&image) {
        assert!(reserve >= WANTED, "main-thread stack reserve is {reserve}");
    } else if cfg!(windows) {
        panic!("a Windows binary has a PE header");
    }
}

/// The real mechanism, on the platform it broke on: the commands the Windows
/// self-test runs must start.
#[cfg(windows)]
#[test]
fn the_binary_parses_its_command_line_on_a_windows_main_thread() {
    let dir = tempfile::tempdir().expect("dir");
    for args in [&["--help"][..], &["providers"][..]] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_kgraf"))
            .env("KGRAF_DIAG_DISABLE", "1")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .expect("run kgraf");
        assert!(out.status.success(), "{args:?}: {}", out.status);
    }
}
