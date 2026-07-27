//! The Microsoft Store update method — scoped to winget's `msstore` source (no MSIX
//! engine of our own). `winget upgrade --source msstore` lists Store updates and
//! `winget upgrade --id <id> --source msstore` applies one. Best-effort under the
//! SYSTEM service: Store apps are per-user and may need the user's Store entitlement,
//! so some updates can only be applied while that user is signed in.

use crate::updater::domain::{
    classify_error, AttemptOutcome, ErrorCategory, Method, UpdateCandidate, Verification,
};
use crate::updater::methods::winget::{clean_winget_output, parse_update_listing, run_winget};
use crate::updater::proc::{INSTALL, LIST};
use crate::updater::verify::{verify_app, VerifyTarget};
use crate::updater::winget_parse::AppUpdate;

/// List Store apps with an available update.
pub async fn list_updates() -> Result<Vec<AppUpdate>, String> {
    let (code, out) = run_winget(
        vec![
            "upgrade".to_string(),
            "--source".to_string(),
            "msstore".to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        LIST,
    )
    .await;
    parse_update_listing(
        "Microsoft Store update listing",
        crate::updater::proc::checked_output("Microsoft Store update listing", code, &out)?,
    )
}

/// Update one Store app via winget's msstore source, then verify by id.
pub async fn attempt(candidate: &UpdateCandidate) -> AttemptOutcome {
    let id = match candidate.package_id.as_deref() {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            return AttemptOutcome::failed(
                Method::MsStore,
                ErrorCategory::NotFound,
                "no Store product id for this app",
            )
        }
    };

    let (code, output) = run_winget(
        vec![
            "upgrade".to_string(),
            "--id".to_string(),
            id.clone(),
            "--exact".to_string(),
            "--source".to_string(),
            "msstore".to_string(),
            "--silent".to_string(),
            "--accept-package-agreements".to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        INSTALL,
    )
    .await;

    let clean = clean_winget_output(&output);
    let mut out = AttemptOutcome::failed(Method::MsStore, ErrorCategory::Unknown, String::new());
    out.exit_code = Some(code);
    if code == 0 {
        let (verification, found) = verify_app(
            &VerifyTarget::Winget { id: id.clone() },
            &candidate.available,
        )
        .await;
        out.verification = verification;
        out.installed_version = (!found.is_empty()).then_some(found);
        out.success = verification != Verification::Mismatch;
        out.category = if out.success {
            None
        } else {
            Some(ErrorCategory::VerifyFailed)
        };
        out.detail = if clean.is_empty() {
            "updated via Microsoft Store".to_string()
        } else {
            clean
        };
    } else {
        out.category = Some(classify_error(Method::MsStore, Some(code), &output));
        out.detail = if clean.is_empty() {
            format!("Store update exited with code {code}")
        } else {
            clean
        };
    }
    out
}
