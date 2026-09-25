//! A one-line description of the machine (OS/build, maker/model, CPU, RAM) so "Ask Eir"
//! can explain the system it is actually running on. Every field is optional; `read()`
//! is platform-specific (Windows registry + `GlobalMemoryStatusEx`, Linux
//! `/etc/os-release` + `/proc` + DMI) but always produces this same shape.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::read;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::read;

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

/// Render the profile as one line, or `None` when nothing could be read.
pub fn describe(p: &MachineProfile) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(product) = &p.product {
        // Windows 11 still reports "Windows 10" in ProductName; the build is authoritative.
        // (A Linux `product` never contains "Windows 10", so this is a no-op there.)
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
    fn describes_a_linux_machine_without_touching_the_windows_11_rewrite() {
        let p = MachineProfile {
            product: Some("Ubuntu 26.04.1 LTS".into()),
            manufacturer: Some("DigitalOcean".into()),
            model: Some("Droplet".into()),
            cpu: Some("AMD EPYC 7402P 24-Core Processor".into()),
            ram_gb: Some(11.7),
            ..Default::default()
        };
        assert_eq!(
            describe(&p).as_deref(),
            Some("Ubuntu 26.04.1 LTS, DigitalOcean Droplet, AMD EPYC 7402P 24-Core Processor, 12 GB RAM")
        );
    }
}
