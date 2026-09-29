//! The `RT_MANIFEST` XML embedded in both `kgraf.exe` and `kgraf-app.exe`.
//!
//! Without an application manifest, a Windows exe with no manifest at all gets the
//! oldest compatibility behavior Windows offers for anything unrecognized: classic
//! (non-themed) common controls, silent truncation of any path over `MAX_PATH` (260
//! chars) even where NTFS and the API both support longer ones, and `GetVersion`-style
//! APIs lying about the running OS to look like an old Windows version to legacy
//! detection code. Three declarations fix this:
//!
//! - `supportedOS`: only the Windows 10/Windows 11 GUID (they share one -- Microsoft
//!   never issued a separate ID for 11) is listed, matching this project's documented
//!   minimum (`scripts/release-local.md`: "a real Windows 10 or later machine"; no
//!   support is claimed, tested or intended for Windows 7/8/8.1/Vista, so their GUIDs
//!   are deliberately omitted rather than copied from every other manifest template).
//! - `Microsoft.Windows.Common-Controls` 6.0.0.0: without this, `tray-icon`'s menus and
//!   any native common-control egui/eframe ends up drawing through fall back to the
//!   unthemed Windows Classic look even on a themed Windows 10/11 desktop.
//! - `longPathAware`: kgraf-cli's `apply_patch`/`graph_slice` tools
//!   (`crates/kgraf-tools`) write and read paths a worker's own instruction picks,
//!   including into deeply nested `.kgraf/worktrees/<name>/...` trees; without this,
//!   a path over 260 characters fails even when the user has enabled Windows' own
//!   long-path support in the registry/policy, because the process itself opted out by
//!   omitting this element.
//!
//! Shared as a single constant (rather than duplicated per binary) so kgraf.exe and
//! kgraf-app.exe carry byte-identical manifests; `crates/kgraf-shell/build.rs`
//! and `crates/kgraf-cli/build.rs` each write it to their own `OUT_DIR` and embed it
//! via `RT_MANIFEST` in the generated `.rc` (`packaging/windows/version_rc.rs`).
pub const MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 and Windows 11 share this one GUID. -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls"
                         version="6.0.0.0" processorArchitecture="*"
                         publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_the_win10_11_supported_os_guid() {
        assert!(MANIFEST_XML.contains("{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"));
    }

    #[test]
    fn depends_on_common_controls_v6() {
        assert!(MANIFEST_XML.contains("Microsoft.Windows.Common-Controls"));
        assert!(MANIFEST_XML.contains(r#"version="6.0.0.0""#));
    }

    #[test]
    fn declares_long_path_awareness() {
        assert!(MANIFEST_XML.contains("<longPathAware"));
        assert!(MANIFEST_XML.contains(">true<"));
    }

    #[test]
    fn is_well_formed_enough_to_have_matched_tags() {
        // No XML parser dependency here (this file is also compiled standalone with
        // plain `rustc` -- see the module doc); a paired open/close count per element
        // name is a cheap, dependency-free sanity check that this is not stray text.
        for tag in [
            "assembly",
            "compatibility",
            "application",
            "dependency",
            "dependentAssembly",
            "windowsSettings",
        ] {
            // `<{tag} ` / `<{tag}>` (a trailing space or `>`), not a bare prefix match:
            // `<assembly` alone would also match `<assemblyIdentity`, which is a
            // different, self-closing element with no `</assemblyIdentity>` to pair.
            let opens = MANIFEST_XML.matches(&format!("<{tag} ")).count()
                + MANIFEST_XML.matches(&format!("<{tag}>")).count();
            let closes = MANIFEST_XML.matches(&format!("</{tag}>")).count();
            assert_eq!(opens, closes, "unbalanced <{tag}> tags");
        }
    }
}
