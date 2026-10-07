//! Machine facts: what Windows, which processor, which tools.
//!
//! The parsers are pure and take the text the Windows tools print
//! (`reg query`, PowerShell `Format-List`, `whoami /groups`), so they are
//! tested with recorded outputs on any host. `collect` (Windows only) runs
//! the tools; every one of them only reads.

use std::collections::BTreeMap;

use serde::Serialize;

/// One `reg query` value: `    Name    REG_SZ    data`.
pub fn parse_reg_query(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if !line.starts_with("    ") {
            continue;
        }
        let parts: Vec<&str> = line.trim().splitn(3, "    ").collect();
        if parts.len() < 2 || !parts[1].trim().starts_with("REG_") {
            continue;
        }
        let name = parts[0].trim();
        let kind = parts[1].trim();
        let data = parts.get(2).map_or("", |d| d.trim());
        let value = if kind == "REG_DWORD" || kind == "REG_QWORD" {
            parse_hex(data).map_or_else(|| data.to_owned(), |n| n.to_string())
        } else {
            data.to_owned()
        };
        out.insert(name.to_owned(), value);
    }
    out
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// PowerShell `Format-List` output: `Key : Value` lines.
pub fn parse_format_list(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(" : ").or_else(|| l.split_once(':')))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, _)| !k.is_empty() && !k.contains(' '))
        .collect()
}

/// Windows 11 keeps `ProductName` "Windows 10 ..." in the registry; the build
/// number tells them apart (22000 is the first Windows 11 build).
pub fn windows_name(product_name: &str, build: u32) -> String {
    if build >= 22000 && product_name.contains("Windows 10") {
        product_name.replacen("Windows 10", "Windows 11", 1)
    } else {
        product_name.to_owned()
    }
}

/// The mandatory label in `whoami /groups`: High (S-1-16-12288) or above
/// means the process runs elevated.
pub fn elevated_from_whoami(groups: &str) -> Option<bool> {
    if groups.contains("S-1-16-12288") || groups.contains("S-1-16-16384") {
        Some(true)
    } else if groups.contains("S-1-16-8192") {
        Some(false)
    } else {
        None
    }
}

/// `IsWow64Process2` machine codes.
pub fn machine_name(code: u16) -> &'static str {
    match code {
        0x8664 => "x86_64",
        0xAA64 => "ARM64",
        0x014c => "x86",
        0 => "unknown",
        _ => "other",
    }
}

/// The SmartScreen and Smart App Control state, from their registry values.
/// Smart App Control blocks unsigned programs, which every bundled test
/// binary is, so it is the first thing to look at when nothing starts.
pub fn smart_app_control(state: Option<&str>) -> &'static str {
    match state {
        Some("0") => "off",
        Some("1") => "on (blocks unsigned programs: the bundled tests cannot start)",
        Some("2") => "evaluation (may block unsigned programs)",
        Some(_) => "unknown value",
        None => "not present",
    }
}

/// The first non-empty line.
pub fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MachineFacts {
    pub windows: Option<String>,
    pub display_version: Option<String>,
    pub build: Option<String>,
    pub edition: Option<String>,
    pub installation_type: Option<String>,
    /// The machine's own architecture.
    pub native_arch: Option<String>,
    /// The architecture this runner and the tests were built for.
    pub binary_arch: String,
    /// True when x86_64 binaries run under emulation on ARM64.
    pub emulated: Option<bool>,
    pub cpu: Option<String>,
    pub cpu_cores: Option<usize>,
    pub ram_gib: Option<f64>,
    pub elevated: Option<bool>,
    pub git_version: Option<String>,
    pub git_bash_path: Option<String>,
    pub git_bash_version: Option<String>,
    pub git_bash_search: Option<String>,
    pub winget_version: Option<String>,
    pub cargo_version: Option<String>,
    pub defender: BTreeMap<String, String>,
    pub smartscreen: Option<String>,
    pub smart_app_control: Option<String>,
    pub developer_mode: Option<bool>,
    pub locale: Option<String>,
    pub ansi_code_page: Option<String>,
    pub work_volume_fs: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Resources {
    pub free_memory_mib: u64,
    pub total_memory_mib: u64,
    pub free_disk_mib: Option<u64>,
    pub disk: Option<String>,
}

/// The mount point that holds `path`: the longest one that is a prefix,
/// compared without case (drive letters).
pub fn disk_for<'a>(path: &str, mounts: &'a [(String, u64)]) -> Option<&'a (String, u64)> {
    let p = path.to_lowercase().replace('/', "\\");
    mounts
        .iter()
        .filter(|(m, _)| {
            let m = m.to_lowercase().replace('/', "\\");
            p.starts_with(&m) || (m == "\\" && p.starts_with('\\'))
        })
        .max_by_key(|(m, _)| m.len())
}

/// Memory and the free space of the disk that holds `path`.
pub fn resources(path: &std::path::Path) -> Resources {
    use sysinfo::{Disks, System};
    let mut sys = System::new();
    sys.refresh_memory();
    let disks = Disks::new_with_refreshed_list();
    let mounts: Vec<(String, u64)> = disks
        .list()
        .iter()
        .map(|d| (d.mount_point().display().to_string(), d.available_space()))
        .collect();
    let hit = disk_for(&path.display().to_string(), &mounts);
    Resources {
        free_memory_mib: sys.available_memory() / (1 << 20),
        total_memory_mib: sys.total_memory() / (1 << 20),
        free_disk_mib: hit.map(|(_, free)| free / (1 << 20)),
        disk: hit.map(|(m, _)| m.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT_VERSION: &str = "\r
HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\r
    SystemRoot    REG_SZ    C:\\WINDOWS\r
    CurrentBuild    REG_SZ    26100\r
    DisplayVersion    REG_SZ    24H2\r
    EditionID    REG_SZ    Professional\r
    InstallationType    REG_SZ    Client\r
    ProductName    REG_SZ    Windows 10 Pro\r
    UBR    REG_DWORD    0x1052\r
    Empty    REG_SZ\r
\r
";

    #[test]
    fn reg_query_values_are_read_with_dwords_as_numbers() {
        let v = parse_reg_query(CURRENT_VERSION);
        assert_eq!(v["CurrentBuild"], "26100");
        assert_eq!(v["EditionID"], "Professional");
        assert_eq!(v["ProductName"], "Windows 10 Pro");
        assert_eq!(v["UBR"], "4178");
        assert_eq!(v["SystemRoot"], "C:\\WINDOWS");
        assert_eq!(v["Empty"], "");
        assert!(
            !v.contains_key("HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
        );
    }

    #[test]
    fn windows_11_is_named_from_the_build_number() {
        assert_eq!(windows_name("Windows 10 Pro", 26100), "Windows 11 Pro");
        assert_eq!(windows_name("Windows 10 Pro", 19045), "Windows 10 Pro");
    }

    #[test]
    fn format_list_output_is_read() {
        let text = "\r\nAMServiceEnabled          : True\r\nRealTimeProtectionEnabled : False\r\nAMProductVersion          : 4.18.25070.5\r\n\r\n";
        let v = parse_format_list(text);
        assert_eq!(v["AMServiceEnabled"], "True");
        assert_eq!(v["RealTimeProtectionEnabled"], "False");
        assert_eq!(v["AMProductVersion"], "4.18.25070.5");
    }

    #[test]
    fn elevation_is_read_from_the_mandatory_label() {
        let medium = "Mandatory Label\\Medium Mandatory Level Label S-1-16-8192";
        let high = "Mandatory Label\\High Mandatory Level Label S-1-16-12288";
        assert_eq!(elevated_from_whoami(medium), Some(false));
        assert_eq!(elevated_from_whoami(high), Some(true));
        assert_eq!(elevated_from_whoami("nothing"), None);
    }

    #[test]
    fn machine_codes_and_smart_app_control_are_named() {
        assert_eq!(machine_name(0xAA64), "ARM64");
        assert_eq!(machine_name(0x8664), "x86_64");
        assert!(smart_app_control(Some("1")).starts_with("on"));
        assert_eq!(smart_app_control(None), "not present");
    }

    #[test]
    fn the_disk_is_the_longest_matching_mount() {
        let mounts = vec![
            ("C:\\".to_owned(), 10),
            ("C:\\mnt\\d".to_owned(), 20),
            ("D:\\".to_owned(), 30),
        ];
        assert_eq!(disk_for("c:\\Users\\x", &mounts).map(|m| m.1), Some(10));
        assert_eq!(disk_for("C:\\mnt\\d\\x", &mounts).map(|m| m.1), Some(20));
        assert_eq!(disk_for("E:\\x", &mounts), None);
    }
}
