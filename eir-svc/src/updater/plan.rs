//! The validated install plan for an AI-found native installer, and the
//! deterministic gate every AI proposal must pass — "AI proposes, Rust disposes".
//! The model only ever suggests a URL/version/args; this module decides whether
//! any of it is allowed to run: https-only, a trusted release host or the app's
//! exact vendor-brand domain, an .exe/.msi file, an allow-listed silent switch,
//! and an optional vendor SHA-256. Ported verbatim with its tests. Pure (no I/O).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstallerKind {
    Exe,
    Msi,
    Zip,
    #[serde(rename = "7z")]
    SevenZ,
    Tar,
    #[serde(rename = "tar.gz")]
    TarGz,
}

impl InstallerKind {
    pub fn is_archive(self) -> bool {
        matches!(self, Self::Zip | Self::SevenZ | Self::Tar | Self::TarGz)
    }
}

/// Untrusted AI output — never used directly; sanitised by [`validate_plan`].
#[derive(Deserialize, Default)]
pub struct InstallPlanRaw {
    // The prompt tells the model to use `null` for fields it can't fill. serde
    // refuses `null` for a plain String (which crashed the whole parse), so these
    // string/array fields accept null|missing as empty via de_null_*.
    #[serde(default, deserialize_with = "de_null_string")]
    pub installer_url: String,
    #[serde(default, deserialize_with = "de_null_string")]
    pub releases_url: String,
    #[serde(default, deserialize_with = "de_null_string")]
    pub archive_installer_path: String,
    #[serde(default, deserialize_with = "de_null_string")]
    pub expected_version: String,
    #[serde(default, deserialize_with = "de_null_vec")]
    pub silent_args: Vec<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default, deserialize_with = "de_null_string")]
    pub publisher: String,
}

/// Deserialize a string that the AI may send as JSON `null` (or omit) — both map
/// to an empty string instead of failing the whole parse.
fn de_null_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Same null-tolerance for a string array (the AI sometimes sends silent_args: null).
fn de_null_vec<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// A server-validated install plan — the only plan the install pipeline trusts.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstallPlan {
    pub name: String,
    pub current: String,
    pub installer_url: String,
    pub host: String,
    pub releases_url: Option<String>,
    pub archive_installer_path: Option<String>,
    pub expected_version: String,
    pub kind: InstallerKind,
    pub silent_args: Vec<String>,
    pub sha256: Option<String>,
    pub expected_publisher: Option<String>,
}

/// Multi-tenant release hosts trusted to serve any vendor's installer. A specific
/// vendor's own domain is accepted separately via host_matches_name.
///
/// These are GitHub's *release-asset* origins only. We deliberately do NOT trust the
/// wildcard `*.github.io` (Pages) or `raw./gist.githubusercontent.com`, because those
/// namespaces serve ARBITRARY user-controlled files — trusting them would let a
/// poisoned AI URL like `attacker.github.io/krita/setup.exe` pass the gate for any
/// app. A vendor that publishes only via Pages is routed through host_matches_name
/// (brand-label equality) or falls back to manual download.
const TRUSTED_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Two-label public suffixes we recognise so the brand label is taken from the
/// right position (e.g. vendor.co.uk -> "vendor", not "co"). Not exhaustive — an
/// unrecognised multi-part TLD just falls back to manual download, which is safe.
const MULTI_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "com.au", "co.nz", "co.jp", "com.br", "co.in", "co.za", "com.tr",
];

/// The brand label of a host: the label immediately left of the public suffix
/// (e.g. download.krita.org -> "krita", app.vendor.co.uk -> "vendor").
fn brand_label(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 2 {
        return None;
    }
    let last2 = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    let suffix_labels = if MULTI_SUFFIXES.contains(&last2.as_str()) {
        2
    } else {
        1
    };
    labels
        .len()
        .checked_sub(suffix_labels + 1)
        .map(|i| labels[i].to_string())
}

fn host_trusted(host: &str) -> bool {
    TRUSTED_HOSTS.contains(&host.to_lowercase().as_str())
}

/// Lowercased alphanumeric token of a string (for app-name/domain matching).
fn alnum_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Whether a vendor domain belongs to the app: its BRAND label must EXACTLY equal
/// the app-name token (whole name or its first brand word). Equality — not a
/// substring test — so lookalikes like obsidian-download.com, notionx.io, or
/// brave.evil.com are rejected; only obsidian.md / krita.org / mozilla.org match.
fn host_matches_name(host: &str, name: &str) -> bool {
    let Some(brand) = brand_label(host).map(|b| alnum_token(&b)) else {
        return false;
    };
    if brand.len() < 4 {
        return false;
    }
    let full = alnum_token(name);
    let first = name
        .split_whitespace()
        .next()
        .map(alnum_token)
        .unwrap_or_default();
    (full.len() >= 4 && brand == full) || (first.len() >= 4 && brand == first)
}

pub fn host_acceptable(host: &str, name: &str) -> bool {
    host_trusted(host) || host_matches_name(host, name)
}

/// `github.com` is a multi-tenant release host: being *on* it proves nothing about
/// which repo an asset belongs to, so the blanket host trust let the AI point a native
/// install at ANY attacker-owned repo. For a `github.com` URL, require the `/owner/repo/`
/// path to correlate with the app name. Alnum containment (looser than the vendor-domain
/// equality) because a niche native-only app's repo name is usually its name; a false
/// negative merely skips the native update (manual/other methods remain).
///
/// This NARROWS the risk (a totally-unrelated `attacker/evil` repo is now rejected) but
/// does NOT eliminate it: a repo *named after* the app (`attacker/krita-fork`) still
/// correlates — no repo-name-only heuristic can tell a hostile fork from a legitimate one
/// (`ferdium/ferdium-app`, `krita/krita-desktop`) without a trusted per-app owner
/// allowlist, which does not exist here. The **Authenticode signature gate + any
/// vendor-published SHA-256 remain the real backstop** for that residual. The opaque
/// `*.githubusercontent.com` asset CDNs carry no repo path and are only reachable as
/// redirect targets, so they stay trusted by host alone (an AI-supplied CDN URL is
/// likewise gated only by the signature/hash).
fn github_repo_correlates(u: &url::Url, name: &str) -> bool {
    let mut segs = u.path_segments().into_iter().flatten();
    let owner = alnum_token(segs.next().unwrap_or(""));
    let repo = alnum_token(segs.next().unwrap_or(""));
    let app = alnum_token(name);
    let first = name
        .split_whitespace()
        .next()
        .map(alnum_token)
        .unwrap_or_default();
    let relates = |seg: &str| -> bool {
        if seg.is_empty() {
            return false;
        }
        (app.len() >= 3 && (seg.contains(app.as_str()) || app.contains(seg)))
            || (first.len() >= 3 && (seg.contains(first.as_str()) || first.contains(seg)))
    };
    relates(&owner) || relates(&repo)
}

/// Strict gate for the initial URL and every redirect hop / final URL: https,
/// no credentials, default port, not a raw IP, not punycode/IDN, and an
/// acceptable host. Returns Err(reason) so callers can surface why a hop failed.
pub fn url_acceptable(u: &url::Url, name: &str) -> Result<(), &'static str> {
    if u.scheme() != "https" {
        return Err("not https");
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("embeds credentials");
    }
    if u.port().is_some() {
        return Err("non-default port");
    }
    let mut host = u.host_str().ok_or("no host")?.to_lowercase();
    // Normalise a rooted FQDN's single trailing dot (`github.com.` == `github.com`),
    // otherwise the exact-match trusted-host check wrongly rejects a legit download.
    if let Some(stripped) = host.strip_suffix('.') {
        host = stripped.to_string();
    }
    // Reject raw IPs — including bracketed IPv6, since host_str() returns "[::1]"
    // which IpAddr::parse would otherwise reject, silently bypassing this guard.
    let bare_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);
    if bare_ip.parse::<std::net::IpAddr>().is_ok() {
        return Err("raw IP host");
    }
    if host.starts_with("xn--") || host.contains(".xn--") {
        return Err("punycode/IDN host");
    }
    if !host_acceptable(&host, name) {
        return Err("untrusted host");
    }
    if host == "github.com" && !github_repo_correlates(u, name) {
        return Err("github repo does not correlate with the app");
    }
    Ok(())
}

pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Keep only known, safe silent-install switches; drop anything with shell
/// metacharacters or whitespace so nothing extra can reach the elevated script.
pub fn sanitise_args(kind: InstallerKind, raw: &[String]) -> Vec<String> {
    const ALLOW_EXE: &[&str] = &[
        "/S",
        "/silent",
        "/verysilent",
        "/quiet",
        "/q",
        "/norestart",
        "/passive",
        "/suppressmsgboxes",
    ];
    const ALLOW_MSI: &[&str] = &[
        "/qn",
        "/quiet",
        "/norestart",
        "/passive",
        "REBOOT=ReallySuppress",
    ];
    const ALLOW_ARCHIVE: &[&str] = &[
        "/S",
        "/silent",
        "/verysilent",
        "/quiet",
        "/q",
        "/norestart",
        "/passive",
        "/suppressmsgboxes",
        "/qn",
        "REBOOT=ReallySuppress",
    ];
    let allow: &[&str] = match kind {
        InstallerKind::Exe => ALLOW_EXE,
        InstallerKind::Msi => ALLOW_MSI,
        InstallerKind::Zip | InstallerKind::SevenZ | InstallerKind::Tar | InstallerKind::TarGz => {
            ALLOW_ARCHIVE
        }
    };
    let mut out: Vec<String> = Vec::new();
    for a in raw {
        let t = a.trim();
        if t.is_empty()
            || t.chars().any(|c| {
                matches!(
                    c,
                    ' ' | '\t' | '\'' | '"' | ';' | '&' | '|' | '>' | '<' | '$' | '`' | '\n' | '\r'
                )
            })
        {
            continue;
        }
        if allow.iter().any(|x| x.eq_ignore_ascii_case(t))
            && !out.iter().any(|o| o.eq_ignore_ascii_case(t))
        {
            out.push(t.to_string());
        }
    }
    out
}

/// Deterministically validate an AI-proposed plan. Pure (no I/O) and unit-tested:
/// the AI only proposes; Rust disposes. Rejection => the caller falls back to a
/// manual browser download.
pub fn validate_plan(
    raw: InstallPlanRaw,
    name: &str,
    current: &str,
) -> Result<InstallPlan, String> {
    let url_str = raw.installer_url.trim().to_string();
    if url_str.is_empty() || url_str.eq_ignore_ascii_case("null") {
        return Err("no direct installer URL".into());
    }
    let parsed = url::Url::parse(&url_str).map_err(|_| "installer URL is not valid".to_string())?;
    if parsed.scheme() != "https" {
        return Err("installer URL is not https".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("installer URL embeds credentials".into());
    }
    if parsed.port().is_some() {
        return Err("installer URL uses a non-default port".into());
    }
    let mut host = parsed
        .host_str()
        .ok_or("installer URL has no host")?
        .to_lowercase();
    if let Some(stripped) = host.strip_suffix('.') {
        host = stripped.to_string();
    }
    let bare_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);
    if bare_ip.parse::<std::net::IpAddr>().is_ok() {
        return Err("installer URL host is a raw IP".into());
    }
    if host.starts_with("xn--") || host.contains(".xn--") {
        return Err("installer URL host is punycode/IDN".into());
    }
    if !host_acceptable(&host, name) {
        return Err(format!(
            "host '{host}' is not a trusted release host or the app's vendor domain"
        ));
    }
    if host == "github.com" && !github_repo_correlates(&parsed, name) {
        return Err(format!(
            "github repo in '{}' does not correlate with app '{name}'",
            parsed.path()
        ));
    }
    let path = parsed.path().to_lowercase();
    let kind = if path.ends_with(".msi") {
        InstallerKind::Msi
    } else if path.ends_with(".exe") {
        InstallerKind::Exe
    } else if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        InstallerKind::TarGz
    } else if path.ends_with(".tar") {
        InstallerKind::Tar
    } else if path.ends_with(".7z") {
        InstallerKind::SevenZ
    } else if path.ends_with(".zip") {
        InstallerKind::Zip
    } else {
        return Err("installer URL does not end in .exe, .msi, .zip, .7z, .tar, or .tar.gz".into());
    };
    let sha256 = match raw
        .sha256
        .as_ref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty() && s != "null")
    {
        Some(s) if is_hex64(&s) => Some(s),
        Some(_) => return Err("provided sha256 is not 64 hex characters".into()),
        None => None,
    };
    let expected_version = raw.expected_version.trim().to_string();
    if expected_version.is_empty() || expected_version.eq_ignore_ascii_case("null") {
        return Err("plan has no expected version".into());
    }
    let releases_url = {
        let r = raw.releases_url.trim();
        if r.starts_with("https://") {
            Some(r.to_string())
        } else {
            None
        }
    };
    let archive_installer_path = {
        let p = raw.archive_installer_path.trim().replace('\\', "/");
        if p.is_empty() || p.eq_ignore_ascii_case("null") {
            None
        } else if !kind.is_archive()
            || p.starts_with('/')
            || p.contains(':')
            || p.split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || !(p.to_lowercase().ends_with(".exe") || p.to_lowercase().ends_with(".msi"))
        {
            return Err("archive installer path is not a safe relative .exe/.msi path".into());
        } else {
            Some(p)
        }
    };
    let expected_publisher = {
        let p = raw.publisher.trim();
        if p.is_empty() || p.eq_ignore_ascii_case("null") {
            None
        } else {
            Some(p.to_string())
        }
    };
    // An MSI must always run silently; if no usable switch survived, use msiexec's
    // standard quiet flags. An .exe with no known switch stays empty and is then
    // routed to manual install (running it hidden would hang).
    let mut silent_args = sanitise_args(kind, &raw.silent_args);
    if kind == InstallerKind::Msi && silent_args.is_empty() {
        silent_args = vec!["/qn".to_string(), "/norestart".to_string()];
    }
    Ok(InstallPlan {
        name: name.to_string(),
        current: current.to_string(),
        installer_url: url_str,
        host,
        releases_url,
        archive_installer_path,
        expected_version,
        kind,
        silent_args,
        sha256,
        expected_publisher,
    })
}

/// Whether a validated plan can be installed unattended. An .exe with no known
/// silent switch is refused (running it hidden would hang) — manual fallback.
pub fn plan_runnable(plan: &InstallPlan) -> bool {
    plan.kind != InstallerKind::Exe || !plan.silent_args.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(url: &str) -> InstallPlanRaw {
        InstallPlanRaw {
            installer_url: url.to_string(),
            releases_url: String::new(),
            archive_installer_path: String::new(),
            expected_version: "2.0.0".to_string(),
            silent_args: vec!["/S".to_string()],
            sha256: None,
            publisher: String::new(),
        }
    }

    #[test]
    fn validate_plan_rejects_uncorrelated_github_repo() {
        // github.com is trusted as a host, but an attacker-proposed repo unrelated to the
        // app must still be rejected — the repo path has to correlate with the app name.
        let err = validate_plan(
            raw("https://github.com/attacker/evil/releases/download/v1/setup.exe"),
            "Krita",
            "1.0.0",
        )
        .unwrap_err();
        assert!(err.contains("github repo"), "got: {err}");
        // The genuine repo correlates (repo == app) and is accepted.
        assert!(validate_plan(
            raw("https://github.com/KDE/krita/releases/download/v5/krita-setup.exe"),
            "Krita",
            "1.0.0",
        )
        .is_ok());
    }

    #[test]
    fn validate_plan_accepts_github_release_exe() {
        let p = validate_plan(
            raw("https://github.com/foo/bar/releases/download/v2/Bar-setup.exe"),
            "Bar App",
            "1.0.0",
        )
        .unwrap();
        assert_eq!(p.kind, InstallerKind::Exe);
        assert_eq!(p.host, "github.com");
        assert_eq!(p.silent_args, vec!["/S".to_string()]);
    }

    #[test]
    fn validated_plan_discards_the_model_supplied_verification_path() {
        let proposed: InstallPlanRaw = serde_json::from_value(serde_json::json!({
            "installer_url": "https://github.com/foo/bar/releases/download/v2/Bar-setup.exe",
            "expected_version": "2.0.0",
            "silent_args": ["/S"],
            "verify_exe": r"\\attacker.example\share\bar.exe"
        }))
        .unwrap();
        let plan = validate_plan(proposed, "Bar App", "1.0.0").unwrap();
        let plan = serde_json::to_value(plan).unwrap();
        assert!(
            plan.get("verify_exe").is_none(),
            "model-supplied filesystem paths must not survive validation"
        );
    }

    #[test]
    fn validate_plan_accepts_github_release_zip() {
        let mut zip = raw("https://github.com/foo/bar/releases/download/v2/Bar-setup.zip");
        zip.archive_installer_path = "setup\\Bar.msi".to_string();
        let p = validate_plan(zip, "Bar App", "1.0.0").unwrap();
        assert_eq!(p.kind, InstallerKind::Zip);
        assert_eq!(p.archive_installer_path.as_deref(), Some("setup/Bar.msi"));

        let mut unsafe_zip = raw("https://github.com/foo/bar/releases/download/v2/Bar-setup.zip");
        unsafe_zip.archive_installer_path = "../Bar.exe".to_string();
        assert!(validate_plan(unsafe_zip, "Bar App", "1.0.0").is_err());
    }

    #[test]
    fn validate_plan_accepts_supported_release_archives() {
        for (suffix, kind) in [
            ("7z", InstallerKind::SevenZ),
            ("tar", InstallerKind::Tar),
            ("tar.gz", InstallerKind::TarGz),
        ] {
            let mut archive = raw(&format!(
                "https://github.com/foo/bar/releases/download/v2/Bar-setup.{suffix}"
            ));
            archive.archive_installer_path = "setup/Bar.exe".to_string();
            let plan = validate_plan(archive, "Bar App", "1.0.0").unwrap();
            assert_eq!(plan.kind, kind);
            assert_eq!(
                plan.archive_installer_path.as_deref(),
                Some("setup/Bar.exe")
            );
        }
    }

    #[test]
    fn install_plan_raw_tolerates_null_fields() {
        // The model is told to use null for fields it can't fill; null on a String
        // field previously crashed the whole parse ("invalid type: null") and forced
        // a manual fallback (the AllTheThings symptom).
        let json = r#"{"installer_url":null,"releases_url":"https://github.com/me/AllTheThings/releases","expected_version":"1.2.3","silent_args":null,"sha256":null,"publisher":null,"verify_exe":null}"#;
        let r: InstallPlanRaw = serde_json::from_str(json).expect("null fields must parse");
        assert_eq!(r.installer_url, "");
        assert!(r.silent_args.is_empty());
        assert_eq!(r.publisher, "");
        // No direct URL -> clean manual routing, not a parse crash.
        assert!(validate_plan(r, "AllTheThings", "1.0.0").is_err());
    }

    #[test]
    fn install_plan_raw_null_publisher_still_validates_github() {
        // A user's own GitHub tool: a direct .exe with null publisher/sha must
        // validate and be installable, not fall back to manual.
        let json = r#"{"installer_url":"https://github.com/me/AllTheThings/releases/download/v1.2.3/AllTheThings-setup.exe","releases_url":null,"expected_version":"1.2.3","silent_args":["/S"],"sha256":null,"publisher":null,"verify_exe":null}"#;
        let r: InstallPlanRaw = serde_json::from_str(json).unwrap();
        let p = validate_plan(r, "AllTheThings", "1.0.0").expect("github exe should validate");
        assert_eq!(p.host, "github.com");
        assert_eq!(p.expected_publisher, None);
        assert_eq!(p.silent_args, vec!["/S".to_string()]);
    }

    #[test]
    fn validate_plan_accepts_vendor_domain_and_defaults_msi_silent() {
        // Vendor domain accepted via the app-name token; MSI with no usable switch
        // falls back to msiexec's quiet flags so it never runs interactively.
        let p = validate_plan(
            raw("https://download.krita.org/installer/krita-x64.msi"),
            "Krita",
            "1.0",
        )
        .unwrap();
        assert_eq!(p.kind, InstallerKind::Msi);
        assert_eq!(
            p.silent_args,
            vec!["/qn".to_string(), "/norestart".to_string()]
        );
    }

    #[test]
    fn validate_plan_rejects_unsafe_urls() {
        assert!(validate_plan(raw("http://github.com/a/b/x.exe"), "X App", "1").is_err()); // not https
        assert!(validate_plan(raw("https://1.2.3.4/x.exe"), "X App", "1").is_err()); // raw IP
        assert!(validate_plan(
            raw("https://totally-unrelated.example/x.exe"),
            "Bar App",
            "1"
        )
        .is_err()); // untrusted host
        assert!(validate_plan(raw("https://user:pw@github.com/a/x.exe"), "X App", "1").is_err());
        // credentials
    }

    #[test]
    fn validate_plan_rejects_bad_sha() {
        let mut r = raw("https://github.com/a/b/x.exe");
        r.sha256 = Some("not-hex".to_string());
        assert!(validate_plan(r, "X App", "1").is_err());
        let mut ok = raw("https://github.com/a/b/x.exe");
        ok.sha256 = Some("A".repeat(64));
        assert_eq!(
            validate_plan(ok, "X App", "1").unwrap().sha256,
            Some("a".repeat(64))
        );
    }

    #[test]
    fn sanitise_args_allow_lists_and_blocks_injection() {
        let exe = sanitise_args(
            InstallerKind::Exe,
            &[
                "/S".into(),
                "/VERYSILENT".into(),
                "; rm -rf".into(),
                "/x && calc".into(),
                "/norestart".into(),
            ],
        );
        assert!(exe.iter().any(|a| a.eq_ignore_ascii_case("/s")));
        assert!(exe.iter().any(|a| a.eq_ignore_ascii_case("/verysilent")));
        assert!(exe.iter().any(|a| a.eq_ignore_ascii_case("/norestart")));
        assert!(!exe
            .iter()
            .any(|a| a.contains("rm") || a.contains("calc") || a.contains('&')));
        // The MSI allow-list is separate; an exe-only switch is dropped.
        assert_eq!(
            sanitise_args(InstallerKind::Msi, &["/qn".into(), "/S".into()]),
            vec!["/qn".to_string()]
        );
    }

    #[test]
    fn host_gate_trusts_github_and_vendor_only() {
        assert!(host_acceptable("github.com", "Anything"));
        assert!(host_acceptable("objects.githubusercontent.com", "Anything"));
        assert!(host_acceptable(
            "release-assets.githubusercontent.com",
            "Anything"
        ));
        // *.github.io (Pages) and raw/gist user content serve arbitrary files — NOT
        // trusted for any app, even though they are GitHub-owned.
        assert!(!host_acceptable("foo.github.io", "Anything"));
        assert!(!host_acceptable("attacker.github.io", "Krita"));
        assert!(!host_acceptable("raw.githubusercontent.com", "Anything"));
        // Exact brand-label match accepts the real vendor domain…
        assert!(host_acceptable("download.krita.org", "Krita"));
        assert!(host_acceptable("obsidian.md", "Obsidian"));
        assert!(host_acceptable("mozilla.org", "Mozilla Firefox"));
        // …but substring lookalikes and brand-as-subdomain tricks are REJECTED.
        assert!(!host_acceptable("obsidian-download.com", "Obsidian"));
        assert!(!host_acceptable("notionx.io", "Notion"));
        assert!(!host_acceptable("get-discord.net", "Discord"));
        assert!(!host_acceptable("krita.evil.com", "Krita"));
        assert!(!host_acceptable("evil.example.com", "Krita"));
    }

    #[test]
    fn hex64_validation() {
        assert!(is_hex64(&"a".repeat(64)));
        assert!(!is_hex64(&"a".repeat(63)));
        assert!(!is_hex64(&"g".repeat(64)));
    }
}
