//! The native update method: for an app no package manager can update, ask the AI
//! for the OFFICIAL installer, validate the plan (Rust disposes), download +
//! hash + signature-gate it, run it as SYSTEM (no UAC), and verify the version
//! moved. The AI only ever proposes a URL/version/args; nothing it returns reaches
//! the shell unchecked.

use crate::ai::client::{extract_json, AiClient};
use crate::updater::config::SignaturePolicy;
use crate::updater::domain::{classify_error, AttemptOutcome, ErrorCategory, Method, Verification};
use crate::updater::download::{download_and_check, Staged};
use crate::updater::plan::{
    plan_runnable, validate_plan, InstallPlan, InstallPlanRaw, InstallerKind,
};
use crate::updater::verify::{verify_app, VerifyTarget};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// CREATE_NO_WINDOW — keep any spawned installer's console hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The prompt that asks the model for one app's official direct installer. The
/// model only proposes; [`validate_plan`] disposes.
fn install_plan_prompt(name: &str, current: &str, note_line: &str) -> String {
    format!(
        "You resolve the OFFICIAL direct download for ONE Windows app so it can be installed \
unattended. Use web search. Use ONLY the vendor's official domain or its official GitHub releases. \
Respond with JSON only — no markdown, no prose.\n\n\
Return exactly this shape:\n\
{{\"installer_url\":\"<https URL ENDING in .exe or .msi — the actual installer FILE for 64-bit \
Windows, machine-wide; NOT a landing/download page; null if you cannot find a direct file with high \
confidence>\",\"releases_url\":\"<official https releases/download page, or null>\",\
\"expected_version\":\"<version this installer produces>\",\"installer_kind\":\"exe|msi\",\
\"silent_args\":[<documented silent switches: NSIS [\\\"/S\\\"]; Inno [\\\"/VERYSILENT\\\",\\\"/NORESTART\\\"]; \
MSI [\\\"/qn\\\",\\\"/norestart\\\"]>],\"sha256\":\"<vendor-published 64-hex hash, or null>\",\
\"publisher\":\"<expected Authenticode signing subject, e.g. 'Mozilla Corporation', or null>\",\
\"verify_exe\":\"<absolute path to an installed .exe whose version proves success, or null>\"}}\n\n\
Rules: if winget can manage this app, set installer_url=null. Never return a URL behind a login, ad \
redirect, or file-locker. If unsure of a DIRECT installer file, set installer_url=null and give \
releases_url only. Respect any [user note] and never contradict it.\n\n\
APP: {name} ({current}){note_line}"
    )
}

/// A validated plan plus the bits the caller needs when it is rejected.
pub struct PlanOutcome {
    pub plan: Option<InstallPlan>,
    pub releases: Option<String>,
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
    let raw: InstallPlanRaw = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            return (
                None,
                None,
                Some(format!("could not parse install plan: {e}")),
            )
        }
    };
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
    note: Option<&str>,
) -> PlanOutcome {
    let note_line = match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!(" [user note: {n}]"),
        None => String::new(),
    };
    let prompt = install_plan_prompt(name, current, &note_line);
    let (content, usage) = match ai.complete(&prompt, model_override).await {
        Ok(v) => v,
        Err(e) => {
            return PlanOutcome {
                plan: None,
                releases: None,
                cost_usd: 0.0,
                reason: Some(e.to_string()),
            }
        }
    };
    let cost_usd = usage.map(|u| u.cost_usd).unwrap_or(0.0);
    let (plan, releases, reason) = plan_from_response(&content, name, current);
    PlanOutcome {
        plan,
        releases,
        cost_usd,
        reason,
    }
}

/// Try to update one app via an AI-found native installer. Returns a structured
/// outcome (method = Native) the orchestrator can act on.
pub async fn update_native(
    ai: &AiClient,
    name: &str,
    current: &str,
    note: Option<&str>,
    policy: SignaturePolicy,
    max_installer_bytes: u64,
    model_override: &str,
) -> AttemptOutcome {
    let mut out = AttemptOutcome::failed(Method::Native, ErrorCategory::Unknown, String::new());
    let planned = make_plan(ai, model_override, name, current, note).await;
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

    let code = run_installer(&staged, plan.kind, &plan.silent_args).await;
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
/// Re-hashes the file immediately before launch (TOCTOU) and applies a 10-minute
/// watchdog. Returns the exit code, or a sentinel: -2 timeout, -3 launch error,
/// -4 the staged file changed since download.
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
    };
    cmd.creation_flags(CREATE_NO_WINDOW).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return -3,
    };
    match tokio::time::timeout(Duration::from_secs(600), child.wait()).await {
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
        let p = install_plan_prompt("Krita", "5.2.0", " [user note: official site]");
        assert!(p.contains("Krita (5.2.0)"));
        assert!(p.contains("user note: official site"));
        assert!(p.contains(".exe or .msi"));
        assert!(p.contains("web search"));
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
            Some("https://github.com/me/Tool/releases")
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
            Some("https://github.com/me/Tool/releases")
        );
    }

    #[test]
    fn plan_from_untrusted_host_is_rejected() {
        let content = r#"{"installer_url":"https://totally-unrelated.example/Tool-setup.exe","releases_url":null,"expected_version":"2.0.0","silent_args":["/S"],"sha256":null,"publisher":null,"verify_exe":null}"#;
        let (plan, _releases, reason) = plan_from_response(content, "Tool", "1.0.0");
        assert!(plan.is_none());
        assert!(reason.unwrap().contains("host"));
    }
}
