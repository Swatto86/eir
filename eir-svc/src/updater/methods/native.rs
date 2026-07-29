//! The native update method: for an app no package manager can update, ask the AI
//! for the OFFICIAL installer, validate the plan (Rust disposes), download +
//! hash + signature-gate it, run it as SYSTEM (no UAC), and verify the version
//! moved. The AI only ever proposes a URL/version/args; nothing it returns reaches
//! the shell unchecked.

use crate::ai::client::{extract_json, AiClient};
use crate::updater::config::SignaturePolicy;
use crate::updater::domain::{
    classify_error, AttemptOutcome, ErrorCategory, Method, UpdateCandidate, Verification,
};
use crate::updater::download::{download_and_check, Staged};
use crate::updater::plan::{
    plan_runnable, sanitise_args, validate_plan, InstallPlan, InstallPlanRaw, InstallerKind,
};
use crate::updater::verify::{verify_app, VerifyTarget};
use sha2::{Digest, Sha256};
use std::path::Path;

/// CREATE_NO_WINDOW — keep any spawned installer's console hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The prompt that asks the model for one app's official direct installer. The
/// model only proposes; [`validate_plan`] disposes.
fn install_plan_prompt(name: &str, current: &str, target: &str, note_line: &str) -> String {
    format!(
        "You resolve the OFFICIAL direct download for ONE Windows app so it can be installed \
unattended. Use web search. Use ONLY the vendor's official domain or its official GitHub releases. \
For GitHub-hosted software, open https://github.com/<owner>/<repo>/releases/latest first; GitHub \
redirects it to the current stable release and its assets. \
Respond with JSON only — no markdown, no prose.\n\n\
Return exactly this shape:\n\
{{\"installer_url\":\"<https URL ENDING in .exe, .msi, .zip, .7z, .tar, or .tar.gz — the actual 64-bit Windows release \
asset; prefer a machine-wide installer; NOT a landing/download page; null if you cannot find one \
with high confidence>\",\"releases_url\":\"<official https releases/download page, or null>\",\
\"archive_installer_path\":\"<exact relative path of the .exe or .msi inside an archive, or null>\",\
\"expected_version\":\"<version this installer produces>\",\"installer_kind\":\"exe|msi|zip|7z|tar|tar.gz\",\
\"silent_args\":[<documented silent switches: NSIS [\\\"/S\\\"]; Inno [\\\"/VERYSILENT\\\",\\\"/NORESTART\\\"]; \
MSI [\\\"/qn\\\",\\\"/norestart\\\"]>],\"sha256\":\"<vendor-published 64-hex hash, or null>\",\
\"publisher\":\"<expected Authenticode signing subject, e.g. 'Mozilla Corporation', or null>\",\
\"verify_exe\":\"<absolute path to an installed .exe whose version proves success, or null>\"}}\n\n\
Rules: if winget can manage this app, set installer_url=null. Never return a URL behind a login, ad \
redirect, or file-locker. If unsure of a direct release asset, set installer_url=null and give \
releases_url only. Prefer a direct .msi or setup .exe; if neither exists, use a ZIP, 7z, TAR, or \
TAR.GZ archive when it contains a Windows .exe/.msi installer. Do not return source-code or \
portable-only archives. The plan must install target version {target} or newer; never return an \
asset for the already-installed version {current}. Respect any \
[user note] and never contradict it.\n\n\
APP: {name} (installed {current}, target {target}){note_line}"
    )
}

fn canonical_github_latest(value: &str) -> Option<String> {
    let value = value.trim().trim_matches([
        '`', '"', '\'', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';',
    ]);
    let parsed = url::Url::parse(value).ok()?;
    if parsed.scheme() != "https"
        || !parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
    {
        return None;
    }
    let segments = parsed.path_segments()?.take(2).collect::<Vec<_>>();
    if segments.len() != 2
        || segments.iter().any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    {
        return None;
    }
    Some(format!(
        "https://github.com/{}/{}/releases/latest",
        segments[0], segments[1]
    ))
}

fn github_latest_in(text: &str) -> Option<String> {
    text.split_whitespace().find_map(canonical_github_latest)
}

fn plan_reaches_target(plan: &InstallPlan, target: &str) -> bool {
    target.trim().is_empty()
        || crate::updater::version::classify_version(&plan.expected_version, target)
            == Verification::Verified
}

/// A validated plan plus the bits the caller needs when it is rejected.
pub struct PlanOutcome {
    pub plan: Option<InstallPlan>,
    pub cost_usd: f64,
    pub reason: Option<String>,
}

/// Parse a model response into a validated plan (or a rejection reason + a manual
/// releases URL). Pure — split out so the parse/validate path is unit-testable
/// against recorded responses without a live provider.
fn plan_from_response(
    content: &str,
    name: &str,
    current: &str,
) -> (Option<InstallPlan>, Option<String>, Option<String>) {
    let json = extract_json(content);
    let mut raw: InstallPlanRaw = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            return (
                None,
                None,
                Some(format!("could not parse install plan: {e}")),
            )
        }
    };
    if let Some(latest) = canonical_github_latest(&raw.releases_url) {
        raw.releases_url = latest;
    }
    let releases_pre = {
        let r = raw.releases_url.trim();
        if r.starts_with("https://") {
            Some(r.to_string())
        } else {
            None
        }
    };
    match validate_plan(raw, name, current) {
        Ok(plan) => {
            let rel = plan.releases_url.clone().or(releases_pre);
            (Some(plan), rel, None)
        }
        Err(e) => (None, releases_pre, Some(e)),
    }
}

/// Ask the AI for an install plan for one app and validate it.
pub async fn make_plan(
    ai: &AiClient,
    model_override: &str,
    name: &str,
    current: &str,
    target: &str,
    note: Option<&str>,
) -> PlanOutcome {
    let note_line = match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!(" [user note: {n}]"),
        None => String::new(),
    };
    let prompt = install_plan_prompt(name, current, target, &note_line);
    let (content, usage) = match ai.complete(&prompt, model_override).await {
        Ok(v) => v,
        Err(e) => {
            return PlanOutcome {
                plan: None,
                cost_usd: 0.0,
                reason: Some(e.to_string()),
            }
        }
    };
    let mut cost_usd = usage.map(|u| u.cost_usd).unwrap_or(0.0);
    let (mut plan, mut releases, mut reason) = plan_from_response(&content, name, current);
    let github_latest = releases
        .as_deref()
        .and_then(canonical_github_latest)
        .or_else(|| {
            plan.as_ref()
                .and_then(|value| canonical_github_latest(&value.installer_url))
        })
        .or_else(|| note.and_then(github_latest_in));
    if plan
        .as_ref()
        .is_some_and(|value| !plan_reaches_target(value, target))
    {
        plan = None;
        reason = Some(format!(
            "AI returned an installer below target version {target}"
        ));
    }
    if plan.is_none() {
        if let Some(latest) = github_latest {
            let correction = format!(
                "{prompt}\n\nThe previous answer was rejected: {}. Open {latest} and inspect that \
latest stable release again. Return only an asset that installs version {target} or newer; never \
use the installed {current} release tag. If that release has no supported Windows installer, \
return installer_url=null and releases_url=\"{latest}\".",
                reason.as_deref().unwrap_or("no direct installer was found")
            );
            match ai.complete(&correction, model_override).await {
                Ok((retry_content, retry_usage)) => {
                    cost_usd += retry_usage.map(|u| u.cost_usd).unwrap_or(0.0);
                    let retried = plan_from_response(&retry_content, name, current);
                    plan = retried.0;
                    releases = retried.1.or(Some(latest));
                    reason = retried.2;
                    if plan
                        .as_ref()
                        .is_some_and(|value| !plan_reaches_target(value, target))
                    {
                        plan = None;
                        reason = Some(format!(
                            "AI returned an installer below target version {target}"
                        ));
                    }
                }
                Err(error) => reason = Some(format!("latest GitHub re-check failed: {error}")),
            }
        }
    }
    // If no direct installer was found but the model surfaced a releases page, fold that
    // manual-download link into the rejection reason (which reaches the UI's failed-native
    // row) instead of discarding it — otherwise the value is computed and tested but never
    // shown.
    let reason = match (&plan, reason, releases) {
        (None, Some(r), Some(url)) => Some(format!("{r} — manual download: {url}")),
        (None, None, Some(url)) => Some(format!("no direct installer — manual download: {url}")),
        (_, r, _) => r,
    };
    PlanOutcome {
        plan,
        cost_usd,
        reason,
    }
}

/// Try to update one app via an AI-found native installer. Returns a structured
/// outcome (method = Native) the orchestrator can act on.
pub async fn update_native(
    ai: &AiClient,
    candidate: &UpdateCandidate,
    policy: SignaturePolicy,
    max_installer_bytes: u64,
    model_override: &str,
) -> AttemptOutcome {
    let mut out = AttemptOutcome::failed(Method::Native, ErrorCategory::Unknown, String::new());
    let planned = make_plan(
        ai,
        model_override,
        &candidate.name,
        &candidate.current,
        &candidate.available,
        candidate.guidance.as_deref(),
    )
    .await;
    out.cost_usd = planned.cost_usd;

    let plan = match planned.plan {
        Some(p) if plan_runnable(&p) => p,
        Some(_) => {
            out.category = Some(ErrorCategory::NotFound);
            out.detail = "no silent-install switch known — manual install only".to_string();
            return out;
        }
        None => {
            out.category = Some(ErrorCategory::NotFound);
            out.detail = planned
                .reason
                .unwrap_or_else(|| "no direct installer found".to_string());
            return out;
        }
    };

    let staged = match download_and_check(&plan, max_installer_bytes, policy).await {
        Ok(s) => s,
        Err(e) => {
            out.category = Some(classify_error(Method::Native, None, &e));
            out.detail = e;
            return out;
        }
    };
    out.signature = Some(staged.signature.display());
    out.sha256 = Some(staged.sha256.clone());

    let mut silent_args = sanitise_args(staged.kind, &plan.silent_args);
    if staged.kind == InstallerKind::Msi && silent_args.is_empty() {
        silent_args = vec!["/qn".to_string(), "/norestart".to_string()];
    }
    if staged.kind == InstallerKind::Exe && silent_args.is_empty() {
        let _ = std::fs::remove_dir_all(&staged.dir);
        out.category = Some(ErrorCategory::NotFound);
        out.detail = "archive installer has no known silent-install switch".to_string();
        return out;
    }

    let code = run_installer(&staged, staged.kind, &silent_args).await;
    let _ = std::fs::remove_dir_all(&staged.dir);
    out.exit_code = Some(code);

    if install_ok(code) {
        let (verification, found) = verify_app(
            &VerifyTarget::ByName {
                name: plan.name.clone(),
                verify_exe: plan.verify_exe.clone(),
            },
            &plan.expected_version,
        )
        .await;
        out.verification = verification;
        out.installed_version = (!found.is_empty()).then_some(found);
        out.success = verification != Verification::Mismatch;
        if out.success {
            out.category = None;
            out.detail = format!(
                "installed{}",
                if code == 3010 {
                    " (reboot required)"
                } else {
                    ""
                }
            );
        } else {
            out.category = Some(ErrorCategory::VerifyFailed);
            out.detail = "installer ran but the new version was not detected".to_string();
        }
    } else {
        out.category = Some(match code {
            -4 => ErrorCategory::HashMismatch, // staged file changed — tampering
            _ => ErrorCategory::InstallerFailed,
        });
        out.detail = match code {
            -4 => {
                "staged installer changed before launch — aborted (possible tampering)".to_string()
            }
            -2 => "installer timed out and was stopped".to_string(),
            -3 => "installer could not be launched".to_string(),
            other => format!("installer exited with code {other}"),
        };
    }
    out
}

/// True when an installer exit code means success (0 = done, 3010 = needs reboot).
fn install_ok(code: i32) -> bool {
    code == 0 || code == 3010
}

/// Hash a file with SHA-256, streaming so a 256 MiB installer doesn't sit in RAM.
async fn sha256_hex_of(path: &Path) -> Result<String, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Run a staged installer as SYSTEM (no UAC — the service is already elevated).
/// Re-hashes the file immediately before launch (TOCTOU) and applies the shared
/// install watchdog. Returns the exit code, or a sentinel: -2 timeout, -3 launch
/// error, -4 the staged file changed since download.
async fn run_installer(staged: &Staged, kind: InstallerKind, args: &[String]) -> i32 {
    match sha256_hex_of(&staged.file).await {
        Ok(got) if got.eq_ignore_ascii_case(&staged.sha256) => {}
        Ok(_) => return -4,
        Err(_) => return -3,
    }
    let mut cmd = match kind {
        InstallerKind::Exe => {
            let mut c = tokio::process::Command::new(&staged.file);
            c.args(args);
            c
        }
        InstallerKind::Msi => {
            let mut c = tokio::process::Command::new("msiexec");
            c.arg("/i").arg(&staged.file).args(args);
            c
        }
        InstallerKind::Zip | InstallerKind::SevenZ | InstallerKind::Tar | InstallerKind::TarGz => {
            return -3
        }
    };
    cmd.creation_flags(CREATE_NO_WINDOW).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return -3,
    };
    match tokio::time::timeout(crate::updater::proc::INSTALL, child.wait()).await {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        Ok(Err(_)) => -3,
        Err(_) => {
            let _ = child.start_kill();
            -2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_the_key_constraints() {
        let p = install_plan_prompt("Krita", "5.2.0", "5.3.0", " [user note: official site]");
        assert!(p.contains("Krita (installed 5.2.0, target 5.3.0)"));
        assert!(p.contains("user note: official site"));
        assert!(p.contains(".exe, .msi, .zip, .7z, .tar, or .tar.gz"));
        assert!(p.contains("web search"));
        assert!(p.contains("https://github.com/<owner>/<repo>/releases/latest"));
        assert!(p.contains("archive_installer_path"));
        assert!(p.contains("if neither exists, use a ZIP, 7z, TAR, or"));
    }

    #[test]
    fn install_ok_accepts_zero_and_reboot() {
        assert!(install_ok(0));
        assert!(install_ok(3010));
        assert!(!install_ok(1603));
        assert!(!install_ok(-4));
    }

    #[test]
    fn plan_from_clean_json_validates() {
        let content = r#"{"installer_url":"https://github.com/me/Tool/releases/download/v2.0.0/Tool-setup.exe","releases_url":"https://github.com/me/Tool/releases","expected_version":"2.0.0","installer_kind":"exe","silent_args":["/S"],"sha256":null,"publisher":null,"verify_exe":null}"#;
        let (plan, releases, reason) = plan_from_response(content, "Tool", "1.0.0");
        assert!(reason.is_none(), "{reason:?}");
        let plan = plan.expect("should validate");
        assert_eq!(plan.expected_version, "2.0.0");
        assert_eq!(plan.silent_args, vec!["/S".to_string()]);
        assert_eq!(
            releases.as_deref(),
            Some("https://github.com/me/Tool/releases/latest")
        );
    }

    #[test]
    fn plan_from_zip_json_validates() {
        let content = r#"{"installer_url":"https://github.com/me/Tool/releases/download/v2.0.0/Tool-setup.zip","releases_url":"https://github.com/me/Tool/releases/latest","archive_installer_path":"setup/Tool.msi","expected_version":"2.0.0","installer_kind":"zip","silent_args":null,"sha256":null,"publisher":null,"verify_exe":null}"#;
        let (plan, _, reason) = plan_from_response(content, "Tool", "1.0.0");
        assert!(reason.is_none(), "{reason:?}");
        let plan = plan.expect("ZIP plan should validate");
        assert_eq!(plan.kind, InstallerKind::Zip);
        assert_eq!(
            plan.archive_installer_path.as_deref(),
            Some("setup/Tool.msi")
        );
    }

    #[test]
    fn plan_from_fenced_and_prose_wrapped_json_validates() {
        // Reasoning models often wrap the JSON in a code fence and commentary.
        let content = "Here's the official installer I found:\n```json\n{\"installer_url\":\"https://download.krita.org/x/krita-x64.msi\",\"releases_url\":null,\"expected_version\":\"5.2.0\",\"installer_kind\":\"msi\",\"silent_args\":null,\"sha256\":null,\"publisher\":null,\"verify_exe\":null}\n```\nThat should work.";
        let (plan, _releases, reason) = plan_from_response(content, "Krita", "5.1.0");
        assert!(reason.is_none(), "{reason:?}");
        let plan = plan.expect("should validate");
        assert_eq!(plan.kind, InstallerKind::Msi);
        // MSI with no usable switch falls back to msiexec's quiet flags.
        assert_eq!(
            plan.silent_args,
            vec!["/qn".to_string(), "/norestart".to_string()]
        );
    }

    #[test]
    fn plan_with_null_installer_url_falls_back_to_manual() {
        let content = r#"{"installer_url":null,"releases_url":"https://github.com/me/Tool/releases","expected_version":"2.0.0","silent_args":null,"sha256":null,"publisher":null,"verify_exe":null}"#;
        let (plan, releases, reason) = plan_from_response(content, "Tool", "1.0.0");
        assert!(plan.is_none());
        assert!(reason.is_some());
        // The releases page is still surfaced for a manual download fallback.
        assert_eq!(
            releases.as_deref(),
            Some("https://github.com/me/Tool/releases/latest")
        );
    }

    #[test]
    fn plan_from_untrusted_host_is_rejected() {
        let content = r#"{"installer_url":"https://totally-unrelated.example/Tool-setup.exe","releases_url":null,"expected_version":"2.0.0","silent_args":["/S"],"sha256":null,"publisher":null,"verify_exe":null}"#;
        let (plan, _releases, reason) = plan_from_response(content, "Tool", "1.0.0");
        assert!(plan.is_none());
        assert!(reason.unwrap().contains("host"));
    }

    #[test]
    fn github_retry_uses_latest_and_rejects_the_installed_release() {
        assert_eq!(
            canonical_github_latest("https://github.com/ryanoasis/nerd-fonts/releases/tag/v3.3.0")
                .as_deref(),
            Some("https://github.com/ryanoasis/nerd-fonts/releases/latest")
        );

        let stale = plan_from_response(
            r#"{"installer_url":"https://github.com/DevelopersCommunity/wix-JetBrainsMonoNF/releases/download/v3.3.0/JetBrainsMono.msi","releases_url":"https://github.com/DevelopersCommunity/wix-JetBrainsMonoNF/releases/tag/v3.3.0","expected_version":"3.3.0","installer_kind":"msi","silent_args":["/qn"],"sha256":null,"publisher":null,"verify_exe":null}"#,
            "JetBrainsMono Nerd Font",
            "3.3.0",
        );
        assert!(!plan_reaches_target(stale.0.as_ref().unwrap(), "3.4.0"));
    }
}
