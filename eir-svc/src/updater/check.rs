//! The "Check" step: build the list of update candidates from every source. winget
//! `upgrade` gives the package-manager candidates; `winget list` + an AI web-search
//! pass covers the apps no package manager can update (correlated-standalone and
//! ARP/unmanaged), which become native candidates. Results are de-duplicated and
//! filtered against the user's ignore list and notes.

use crate::ai::client::{extract_json, AiClient};
use crate::updater::config::UpdaterConfig;
use crate::updater::domain::{Method, UpdateCandidate};
use crate::updater::methods::{choco, msstore, scoop, winget};
use crate::updater::names::{clean_app_name, match_installed};
use crate::updater::version::is_newer;
use crate::updater::winget_parse::parse_unmanaged;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Cap on apps sent to the AI in one batch, to bound cost/latency.
const AI_CHECK_CAP: usize = 20;

/// The result of a full check.
pub struct CheckResult {
    pub candidates: Vec<UpdateCandidate>,
    pub cost_usd: f64,
    /// Human-readable notes (truncation, AI-check failures) for the UI.
    pub notes: Vec<String>,
}

fn ignored(cfg: &UpdaterConfig, id: &str) -> bool {
    cfg.ignored.iter().any(|ig| ig.eq_ignore_ascii_case(id))
}

/// Add a manager candidate if it isn't ignored or already covered by an
/// earlier (more-preferred) manager. The app's primary method is `primary`; the
/// native installer is appended as a self-healing fallback when available.
#[allow(clippy::too_many_arguments)]
fn push_candidate(
    out: &mut Vec<UpdateCandidate>,
    seen: &mut HashSet<String>,
    cfg: &UpdaterConfig,
    native_avail: bool,
    name: &str,
    current: &str,
    available: &str,
    package_id: Option<String>,
    primary: Method,
) {
    let id = clean_app_name(name).to_lowercase();
    if id.is_empty() || ignored(cfg, &id) || !seen.insert(id.clone()) {
        return;
    }
    let mut methods = vec![primary];
    if native_avail && primary != Method::Native {
        methods.push(Method::Native);
    }
    out.push(UpdateCandidate {
        id,
        name: name.to_string(),
        current: current.to_string(),
        available: available.to_string(),
        package_id,
        methods,
    });
}

/// Collect every update candidate across the available methods, de-duplicated by app
/// identity (earlier, more-preferred managers win) and filtered by the ignore list.
pub async fn collect(
    ai: Option<&AiClient>,
    cfg: &UpdaterConfig,
    model_override: &str,
    available: &[Method],
) -> CheckResult {
    let native_avail = available.contains(&Method::Native);
    let mut candidates: Vec<UpdateCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut notes: Vec<String> = Vec::new();
    let mut cost = 0.0;
    // Names any manager already covers, so the AI check skips them.
    let mut managed: HashSet<String> = HashSet::new();

    if available.contains(&Method::Winget) {
        for u in winget::list_updates().await {
            managed.insert(u.name.to_lowercase());
            push_candidate(
                &mut candidates,
                &mut seen,
                cfg,
                native_avail,
                &u.name,
                &u.current,
                &u.available,
                Some(u.id.clone()),
                Method::Winget,
            );
        }
    }
    if available.contains(&Method::Choco) {
        for u in choco::list_outdated().await {
            managed.insert(u.name.to_lowercase());
            push_candidate(
                &mut candidates,
                &mut seen,
                cfg,
                native_avail,
                &u.name,
                &u.current,
                &u.available,
                Some(u.name.clone()),
                Method::Choco,
            );
        }
    }
    if available.contains(&Method::Scoop) {
        for u in scoop::list_outdated().await {
            managed.insert(u.name.to_lowercase());
            push_candidate(
                &mut candidates,
                &mut seen,
                cfg,
                native_avail,
                &u.name,
                &u.current,
                &u.available,
                Some(u.name.clone()),
                Method::Scoop,
            );
        }
    }
    if available.contains(&Method::MsStore) {
        for u in msstore::list_updates().await {
            managed.insert(u.name.to_lowercase());
            push_candidate(
                &mut candidates,
                &mut seen,
                cfg,
                native_avail,
                &u.name,
                &u.current,
                &u.available,
                Some(u.id.clone()),
                Method::MsStore,
            );
        }
    }

    // The AI web-search pass over apps no manager covers -> native candidates.
    if native_avail {
        if let Some(ai) = ai {
            match check_unmanaged(ai, cfg, model_override, &managed).await {
                Ok((native_cands, c, note)) => {
                    cost += c;
                    if let Some(n) = note {
                        notes.push(n);
                    }
                    for cand in native_cands {
                        if seen.insert(cand.id.clone()) {
                            candidates.push(cand);
                        }
                    }
                }
                Err(e) => notes.push(format!("couldn't check non-winget apps: {e}")),
            }
        }
    }

    CheckResult {
        candidates,
        cost_usd: cost,
        notes,
    }
}

#[derive(Deserialize)]
struct AiResp {
    updates: Vec<AiUpdateRaw>,
}

#[derive(Deserialize)]
struct AiUpdateRaw {
    name: String,
    #[serde(default)]
    current: String,
    #[serde(default)]
    latest: String,
}

/// Ask the AI which unmanaged apps have a newer version, and turn the verified ones
/// into native candidates.
async fn check_unmanaged(
    ai: &AiClient,
    cfg: &UpdaterConfig,
    model_override: &str,
    managed: &HashSet<String>,
) -> Result<(Vec<UpdateCandidate>, f64, Option<String>), String> {
    let list_out = tokio::task::spawn_blocking(|| {
        std::process::Command::new("winget")
            .args([
                "list",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("winget is not available: {e}"))?;

    let mut apps = parse_unmanaged(&String::from_utf8_lossy(&list_out.stdout), managed);
    apps.retain(|(n, _)| !ignored(cfg, &n.to_lowercase()));
    let total = apps.len();
    let mut note = None;
    if apps.len() > AI_CHECK_CAP {
        apps.truncate(AI_CHECK_CAP);
        note = Some(format!(
            "Checked the first {AI_CHECK_CAP} of {total} apps winget doesn't manage."
        ));
    }
    if apps.is_empty() {
        return Ok((vec![], 0.0, None));
    }

    let app_lines = apps
        .iter()
        .map(|(n, v)| match cfg.notes.get(&n.to_lowercase()) {
            Some(note) if !note.is_empty() => format!("- {n} ({v}) [user note: {note}]"),
            _ => format!("- {n} ({v})"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are an application update checker. Below are installed Windows applications with their \
current versions. Use web search to find each one's latest STABLE release from its official source. \
Return ONLY the apps that have a NEWER version available.\n\n\
Respect any [user note]: it may say an app is custom/self-built or give its real release source — \
follow that guidance and do NOT report an update that contradicts the note.\n\n\
Respond ONLY with JSON, no markdown:\n\
{{\"updates\":[{{\"name\":\"<app>\",\"current\":\"<installed>\",\"latest\":\"<newer version>\",\"url\":\"<official download or releases page URL>\"}}]}}\n\
Omit apps that are already current or that you cannot verify. Only include real, verified versions.\n\n\
INSTALLED APPS:\n{app_lines}"
    );

    let (content, usage) = ai
        .complete(&prompt, model_override)
        .await
        .map_err(|e| e.to_string())?;
    let cost = usage.map(|u| u.cost_usd).unwrap_or(0.0);
    let resp: AiResp = serde_json::from_str(extract_json(&content))
        .map_err(|e| format!("could not parse update list: {e}"))?;

    let installed: HashMap<String, String> = apps
        .iter()
        .map(|(n, v)| (n.to_lowercase(), v.clone()))
        .collect();
    let candidates = native_candidates_from(&resp.updates, &installed, cfg);
    Ok((candidates, cost, note))
}

/// Pure: turn the AI's reported updates into native candidates, keeping only those
/// strictly newer than what is actually installed and not on the ignore list, and
/// stamping each with the authoritative installed version. Split out so the
/// filtering is unit-testable without a live provider.
fn native_candidates_from(
    updates: &[AiUpdateRaw],
    installed: &HashMap<String, String>,
    cfg: &UpdaterConfig,
) -> Vec<UpdateCandidate> {
    let mut out = Vec::new();
    for u in updates {
        if u.name.trim().is_empty() || u.latest.trim().is_empty() {
            continue;
        }
        let cur = match_installed(installed, &u.name)
            .cloned()
            .unwrap_or_else(|| u.current.clone());
        if !is_newer(&u.latest, &cur) {
            continue;
        }
        let id = u.name.to_lowercase();
        if ignored(cfg, &id) {
            continue;
        }
        out.push(UpdateCandidate {
            id,
            name: u.name.clone(),
            current: cur,
            available: u.latest.clone(),
            package_id: None,
            methods: vec![Method::Native],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(name: &str, current: &str, latest: &str) -> AiUpdateRaw {
        AiUpdateRaw {
            name: name.to_string(),
            current: current.to_string(),
            latest: latest.to_string(),
        }
    }

    #[test]
    fn native_candidates_keep_only_strictly_newer_and_respect_ignore() {
        let installed: HashMap<String, String> = [
            ("obsidian".to_string(), "1.5.0".to_string()),
            ("krita".to_string(), "5.2.0".to_string()),
            ("oldtool".to_string(), "2.9.0".to_string()),
        ]
        .into_iter()
        .collect();
        let cfg = UpdaterConfig {
            ignored: vec!["krita".to_string()],
            ..UpdaterConfig::default()
        };
        let updates = vec![
            upd("Obsidian", "1.5.0", "1.6.0"), // newer -> kept
            upd("Krita", "5.2.0", "5.3.0"),    // newer but ignored -> dropped
            upd("OldTool", "2.9.0", "2.7.5"),  // AI hallucinated an older "update" -> dropped
            upd("Empty", "1.0", ""),           // no latest -> dropped
        ];
        let cands = native_candidates_from(&updates, &installed, &cfg);
        let names: Vec<&str> = cands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Obsidian"]);
        // The kept candidate carries the authoritative installed version + target.
        assert_eq!(cands[0].current, "1.5.0");
        assert_eq!(cands[0].available, "1.6.0");
        assert_eq!(cands[0].methods, vec![Method::Native]);
    }
}
