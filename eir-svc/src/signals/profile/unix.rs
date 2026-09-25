//! Linux machine profile: `/etc/os-release`, `/proc/cpuinfo`, `/proc/meminfo`, and
//! `/sys/class/dmi/id/*`. Every read is best-effort — a missing file, an unreadable DMI
//! entry (some fields need root; eir-svc runs as root under systemd, but this must
//! degrade gracefully in a container/CI sandbox without DMI), or a parse failure simply
//! leaves that field `None`.

use super::MachineProfile;
use std::fs;

fn os_release_value(contents: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    contents.lines().find_map(|line| {
        line.strip_prefix(prefix.as_str())
            .map(|value| value.trim().trim_matches('"').to_string())
            .filter(|value| !value.is_empty())
    })
}

fn os_description() -> Option<String> {
    let contents = fs::read_to_string("/etc/os-release").ok()?;
    os_release_value(&contents, "PRETTY_NAME").or_else(|| os_release_value(&contents, "NAME"))
}

/// Skip the generic OEM defaults a DIY/virtualised board reports, mirroring the
/// Windows side's own "System Product Name" placeholder filter.
fn dmi_field(name: &str) -> Option<String> {
    let value = fs::read_to_string(format!("/sys/class/dmi/id/{name}")).ok()?;
    let value = value.trim();
    (!value.is_empty() && !value.eq_ignore_ascii_case("to be filled by o.e.m."))
        .then(|| value.to_string())
}

fn parse_cpu_model(cpuinfo: &str) -> Option<String> {
    for line in cpuinfo.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == "model name" {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn cpu_model() -> Option<String> {
    parse_cpu_model(&fs::read_to_string("/proc/cpuinfo").ok()?)
}

fn parse_total_ram_gb(meminfo: &str) -> Option<f64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb as f64 / (1024.0 * 1024.0));
        }
    }
    None
}

fn total_ram_gb() -> Option<f64> {
    parse_total_ram_gb(&fs::read_to_string("/proc/meminfo").ok()?)
}

/// Blocking `/proc`, `/sys` and `/etc` reads — call from `spawn_blocking`.
pub fn read() -> MachineProfile {
    MachineProfile {
        product: os_description(),
        display_version: None,
        build: None,
        ubr: None,
        manufacturer: dmi_field("sys_vendor"),
        model: dmi_field("product_name"),
        cpu: cpu_model(),
        ram_gb: total_ram_gb(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_prefers_pretty_name_then_falls_back_to_name() {
        let sample =
            "NAME=\"Ubuntu\"\nVERSION=\"26.04.1 LTS\"\nPRETTY_NAME=\"Ubuntu 26.04.1 LTS\"\n";
        assert_eq!(
            os_release_value(sample, "PRETTY_NAME").as_deref(),
            Some("Ubuntu 26.04.1 LTS")
        );
        assert_eq!(os_release_value(sample, "NAME").as_deref(), Some("Ubuntu"));
        assert_eq!(os_release_value(sample, "MISSING"), None);
    }

    #[test]
    fn cpu_model_reads_the_first_model_name_line() {
        let sample =
            "processor\t: 0\nmodel name\t: AMD EPYC 7402P 24-Core Processor\ncache size\t: 512 KB\n";
        assert_eq!(
            parse_cpu_model(sample).as_deref(),
            Some("AMD EPYC 7402P 24-Core Processor")
        );
        assert_eq!(parse_cpu_model(""), None);
    }

    #[test]
    fn total_ram_parses_kb_to_gb() {
        let sample = "MemTotal:       12345678 kB\nMemFree:        1000 kB\n";
        let gb = parse_total_ram_gb(sample).expect("ram parsed");
        assert!((gb - 11.773).abs() < 0.01, "got {gb}");
        assert_eq!(parse_total_ram_gb("garbage"), None);
    }
}
