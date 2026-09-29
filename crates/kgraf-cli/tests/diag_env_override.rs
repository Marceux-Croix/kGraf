//! `kgraf`'s `main` must resolve its diagnostics path through
//! `kgraf_diag::resolve_path` (which honors `KGRAF_DIAG_PATH`/
//! `KGRAF_DIAG_DISABLE`), not `kgraf_diag::default_path` (the real per-user
//! path, unconditionally). Every other test
//! here sets `KGRAF_DIAG_DISABLE` and asserts nothing happens, which a
//! regressed call site (`default_path` instead of `resolve_path`) would pass
//! too, since disabling still does nothing either way; it does not prove
//! `KGRAF_DIAG_PATH` is actually read. This test asserts the positive: a
//! redirected path receives a real record, which only happens if `main`
//! still calls `resolve_path`.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn kgraf_diag_path_is_actually_used_by_the_real_binary() {
    let dir = tempfile::tempdir().expect("dir");
    let diag_path: PathBuf = dir.path().join("diagnostics.jsonl");
    assert!(!diag_path.exists());

    let out = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .args(["-C", &dir.path().to_string_lossy(), "providers"])
        .env("KGRAF_DIAG_PATH", &diag_path)
        .env_remove("KGRAF_DIAG_DISABLE")
        .output()
        .expect("run kgraf");
    assert!(out.status.success(), "{:?}", out.status);

    assert!(
        diag_path.exists(),
        "KGRAF_DIAG_PATH was set to {}, but main never wrote it; if the init call site in \
         main.rs reverted from kgraf_diag::resolve_path to kgraf_diag::default_path, this is \
         exactly what would happen (silently: the command still succeeds, it just always uses \
         the real per-user path instead)",
        diag_path.display()
    );
    let records = kgraf_diag::store::read_all(&diag_path).expect("read diagnostics");
    assert!(
        !records.is_empty(),
        "the redirected file exists but holds no records: {records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| matches!(&r.event, kgraf_diag::DiagEvent::Launch { feature: Some(f) } if f == "providers")),
        "expected a Launch{{feature: \"providers\"}} record; got {records:?}"
    );
}
