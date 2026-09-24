//! A one-line description of the machine (Windows edition/build, maker/model, CPU, RAM)
//! so "Ask Eir" can explain the system it is actually running on. Read from the registry
//! and `GlobalMemoryStatusEx` on demand; every field is optional.

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const CPU: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
const BIOS: &str = r"HARDWARE\DESCRIPTION\System\BIOS";

#[derive(Default)]
pub struct MachineProfile {
    pub product: Option<String>,
    pub display_version: Option<String>,
    pub build: Option<String>,
    pub ubr: Option<u32>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub cpu: Option<String>,
    pub ram_gb: Option<f64>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn read_string(subkey: &str, value: &str) -> Option<String> {
    let (subkey_w, value_w) = (wide(subkey), wide(value));
    let mut buf = [0u16; 256];
    let mut len = u32::try_from(std::mem::size_of_val(&buf)).ok()?;
    // SAFETY: the buffer and its byte length describe the same live stack array.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..chars]);
    let s = s.trim_end_matches('\0').trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    let (subkey_w, value_w) = (wide(subkey), wide(value));
    let mut data = 0u32;
    let mut len = u32::try_from(std::mem::size_of::<u32>()).ok()?;
    // SAFETY: `data` is a live u32 and `len` is its size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut data).cast()),
            Some(&mut len),
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

fn total_ram_gb() -> Option<f64> {
    let mut mem = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `mem` is initialised with its own size, as the API requires.
    unsafe { GlobalMemoryStatusEx(&mut mem) }.ok()?;
    Some(mem.ullTotalPhys as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Blocking registry/API reads — call from `spawn_blocking`.
pub fn read() -> MachineProfile {
    MachineProfile {
        product: read_string(CURRENT_VERSION, "ProductName"),
        display_version: read_string(CURRENT_VERSION, "DisplayVersion"),
        build: read_string(CURRENT_VERSION, "CurrentBuild"),
        ubr: read_dword(CURRENT_VERSION, "UBR"),
        manufacturer: read_string(BIOS, "SystemManufacturer"),
        model: read_string(BIOS, "SystemProductName"),
        cpu: read_string(CPU, "ProcessorNameString"),
        ram_gb: total_ram_gb(),
    }
}

/// Render the profile as one line, or `None` when nothing could be read.
pub fn describe(p: &MachineProfile) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(product) = &p.product {
        // Windows 11 still reports "Windows 10" in ProductName; the build is authoritative.
        let is_11 = p
            .build
            .as_deref()
            .and_then(|b| b.parse::<u32>().ok())
            .is_some_and(|b| b >= 22_000);
        let mut os = if is_11 {
            product.replacen("Windows 10", "Windows 11", 1)
        } else {
            product.clone()
        };
        if let Some(v) = &p.display_version {
            os.push_str(&format!(" {v}"));
        }
        match (&p.build, p.ubr) {
            (Some(b), Some(u)) => os.push_str(&format!(" (build {b}.{u})")),
            (Some(b), None) => os.push_str(&format!(" (build {b})")),
            _ => {}
        }
        parts.push(os);
    }
    let device = [p.manufacturer.as_deref(), p.model.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.eq_ignore_ascii_case("System Product Name"))
        .collect::<Vec<_>>()
        .join(" ");
    if !device.is_empty() {
        parts.push(device);
    }
    if let Some(cpu) = &p.cpu {
        parts.push(cpu.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    if let Some(gb) = p.ram_gb {
        parts.push(format!("{gb:.0} GB RAM"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_windows_11_from_its_build_and_skips_missing_parts() {
        let p = MachineProfile {
            product: Some("Windows 10 Pro".into()),
            display_version: Some("24H2".into()),
            build: Some("26100".into()),
            ubr: Some(4652),
            manufacturer: Some("Dell Inc.".into()),
            model: Some("XPS 8960".into()),
            cpu: Some("13th Gen Intel(R) Core(TM) i7-13700   ".into()),
            ram_gb: Some(31.7),
        };
        assert_eq!(
            describe(&p).as_deref(),
            Some(
                "Windows 11 Pro 24H2 (build 26100.4652), Dell Inc. XPS 8960, \
                 13th Gen Intel(R) Core(TM) i7-13700, 32 GB RAM"
            )
        );
        let win10 = MachineProfile {
            product: Some("Windows 10 Home".into()),
            build: Some("19045".into()),
            ..Default::default()
        };
        assert_eq!(
            describe(&win10).as_deref(),
            Some("Windows 10 Home (build 19045)")
        );
        assert_eq!(describe(&MachineProfile::default()), None);
    }

    #[test]
    fn reads_this_machine() {
        // Every supported Windows has a product name and build in the registry.
        let p = read();
        assert!(p.product.is_some() && p.build.is_some());
        assert!(describe(&p).is_some_and(|d| d.contains("build")));
    }
}
