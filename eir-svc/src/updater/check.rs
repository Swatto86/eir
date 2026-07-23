//! The "Check" step: build the list of update candidates from every source. winget
//! `upgrade` gives the package-manager candidates; `winget list` + an AI web-search
//! pass covers the apps no package manager can update (correlated-standalone and
//! ARP/unmanaged), which become native candidates. Results are de-duplicated and
//! filtered against the user's ignore list and notes.

use crate::ai::client::{extract_json, AiClient};
use crate::updater::config::UpdaterConfig;
use crate::updater::domain::{Method, UpdateCandidate};
use crate::updater::methods::{choco, detect, msstore, scoop, winget};
use crate::updater::names::{clean_app_name, match_installed};
use crate::updater::proc::{self, LIST};
use crate::updater::version::is_newer;
use crate::updater::winget_parse::parse_unmanaged;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

/// Cap on apps sent to the AI in one batch, to bound cost/latency.
const AI_CHECK_CAP: usize = 20;

/// The result of a full check.
pub struct CheckResult {
    pub candidates: Vec<UpdateCandidate>,
    pub cost_usd: f64,
    /// Human-readable notes (truncation, AI-check failures) for the UI.
    pub notes: Vec<String>,
}

/// Apps that update themselves and reliably fight or hang package managers, so the
/// updater never tries to manage them. Discord (a Squirrel per-user installer) hangs
/// `choco upgrade` for the full INSTALL timeout and, once it has self-updated, choco's
/// stale version DB makes it retry every cycle. Matched against the base id (any choco
/// package suffix stripped). Extend conservatively — only apps that genuinely keep
/// themselves current, so skipping them is safe.
const SELF_UPDATING: &[&str] = &["discord"];

/// Strip a Chocolatey package suffix so `discord.install` and `discord` share one
/// identity (`discord`). Choco splits many apps into `<name>` / `<name>.install` /
/// `.portable` / `.app` packages; without this they are treated as separate candidates
/// and a skip/ignore on one misses the others. `pub(crate)` so the self-improvement
/// learner keys on the same identity.
pub(crate) fn base_id(id: &str) -> &str {
    for suffix in [".install", ".portable", ".app", ".commandline"] {
        if let Some(stripped) = id.strip_suffix(suffix) {
            return stripped;
        }
    }
    id
}

/// Whether a candidate id should be skipped: the `SELF_UPDATING` seed, a self-updater
/// the machine has *learned* (`learned`, keyed by base id), or the user's ignore list
/// (the exact id or its base, so ignoring "discord" also covers "discord.install").
fn should_skip(cfg: &UpdaterConfig, learned: &HashSet<String>, id: &str) -> bool {
    let base = base_id(id);
    SELF_UPDATING.contains(&base)
        || learned.contains(base)
        || cfg
            .ignored
            .iter()
            .any(|ig| ig.eq_ignore_ascii_case(id) || ig.eq_ignore_ascii_case(base))
}

/// Add a manager candidate if it isn't ignored or already covered by an
/// earlier (more-preferred) manager. The app's primary method is `primary`; the
/// native installer is appended as a self-healing fallback when available.
#[allow(clippy::too_many_arguments)]
fn push_candidate(
    out: &mut Vec<UpdateCandidate>,
    seen: &mut HashSet<String>,
    cfg: &UpdaterConfig,
    learned: &HashSet<String>,
    native_avail: bool,
    name: &str,
    current: &str,
    available: &str,
    package_id: Option<String>,
    primary: Method,
) {
    let id = clean_app_name(name).to_lowercase();
    if id.is_empty() || should_skip(cfg, learned, &id) || !seen.insert(id.clone()) {
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
    pool: &SqlitePool,
    ai: Option<&AiClient>,
    cfg: &UpdaterConfig,
    model_override: &str,
    available: &[Method],
    learned_skips: &HashSet<String>,
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
                learned_skips,
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
                learned_skips,
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
                learned_skips,
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
                learned_skips,
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
            let winget_list_available =
                available.contains(&Method::Winget) && detect::winget_available();
            match check_unmanaged(
                pool,
                ai,
                cfg,
                model_override,
                &managed,
                learned_skips,
                winget_list_available,
            )
            .await
            {
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
                Err(e) => notes.push(format!("couldn't check non-manager apps: {e}")),
            }
        } else {
            notes.push(
                "AI provider not configured — only package-manager apps are checked.".to_string(),
            );
        }
    } else {
        notes.push(
            "AI-found installers disabled — only package-manager apps are checked.".to_string(),
        );
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
    latest: String,
}

/// Pure: sort unmanaged apps stalest-first by last AI-check time and return up to
/// `cap` of them. Never-checked apps sort first (key 0), then oldest checks.
pub fn select_unmanaged_batch(
    apps: &mut Vec<(String, String)>,
    last_check: &HashMap<String, i64>,
    cap: usize,
) -> Vec<(String, String)> {
    apps.sort_by_key(|(n, _)| last_check.get(&n.to_lowercase()).copied().unwrap_or(0));
    apps.drain(..cap.min(apps.len())).collect()
}

/// Ask the AI which unmanaged apps have a newer version, and turn the verified ones
/// into native candidates. The installed-app inventory is built from the registry
/// Uninstall keys (HKLM, Wow6432Node, per-user) and merged with `winget list` when
/// winget is present. The batch is sorted stalest-first by last AI-check time, capped
/// at `AI_CHECK_CAP`, and each checked app is recorded so the tail is reached across
/// cycles.
async fn check_unmanaged(
    pool: &SqlitePool,
    ai: &AiClient,
    cfg: &UpdaterConfig,
    model_override: &str,
    managed: &HashSet<String>,
    learned_skips: &HashSet<String>,
    winget_list_available: bool,
) -> Result<(Vec<UpdateCandidate>, f64, Option<String>), String> {
    // Build the installed-app inventory from winget (when available) and the registry.
    let mut apps: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let note = if winget_list_available {
        let mut cmd = std::process::Command::new("winget");
        cmd.args([
            "list",
            "--accept-source-agreements",
            "--disable-interactivity",
        ]);
        let (_code, list_text) = proc::run_capped_cmd(cmd, LIST).await;
        let winget_apps = parse_unmanaged(&list_text, managed);
        for (n, v) in winget_apps {
            if seen.insert(n.to_lowercase()) {
                apps.push((n, v));
            }
        }
        None
    } else {
        Some(
            "winget not available — using registry inventory only for non-manager apps."
                .to_string(),
        )
    };

    // Merge in registry inventory as a fallback/supplement. Registry version is
    // authoritative for installed version, so it takes precedence when a name exists
    // in both sources (we skip the winget duplicate here).
    for (n, v) in crate::updater::inventory::list_installed().await {
        let key = n.to_lowercase();
        if managed.contains(&key) || should_skip(cfg, learned_skips, &key) || !seen.insert(key) {
            continue;
        }
        apps.push((n, v));
    }

    // Fair rotation: stalest check time first, never-checked first, so the tail isn't
    // permanently starved. The cap is applied here, not at collection, because the
    // AI call is the expensive step and we want it to sweep across the whole set.
    let last_check = crate::updater::history::last_check_times(pool)
        .await
        .unwrap_or_default();
    let total = apps.len();
    let checked = select_unmanaged_batch(&mut apps, &last_check, AI_CHECK_CAP);
    let note = if total > AI_CHECK_CAP {
        let cap_note = format!(
            "Checking {} of {total} non-manager apps — stalest first.",
            checked.len()
        );
        Some(match note {
            Some(n) => format!("{n} {cap_note}"),
            None => cap_note,
        })
    } else {
        note
    };
    if checked.is_empty() {
        return Ok((vec![], 0.0, note));
    }

    let app_lines = checked
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

    // Record the check time for every app we sent, so the next cycle rotates past them
    // even if the AI reports no update.
    for (n, _) in &checked {
        let _ = crate::updater::history::record_check(pool, &n.to_lowercase()).await;
    }

    let installed: HashMap<String, String> = checked
        .iter()
        .map(|(n, v)| (n.to_lowercase(), v.clone()))
        .collect();
    let resp: AiResp = serde_json::from_str(extract_json(&content))
        .map_err(|e| format!("could not parse update list: {e}"))?;

    let candidates = native_candidates_from(&resp.updates, &installed, cfg, learned_skips);
    Ok((candidates, cost, note))
}

/// Pure: turn the AI's reported updates into native candidates, keeping only those
/// strictly newer than what is actually installed and not on the ignore list, and
/// stamping each with the authoritative installed version. Split out so the
/// filtering is unit-testable without a live provider.
///
/// Identity is anchored to the machine: an update whose name does not resolve to an
/// actually-installed app (the real `winget list` set) is DROPPED. This preserves the
/// "native installs only ever UPDATE apps the machine already has" invariant — without
/// it, the (untrusted) AI could name a fabricated app and thereby choose an arbitrary
/// vendor domain that the name-keyed host gate would then accept.
fn native_candidates_from(
    updates: &[AiUpdateRaw],
    installed: &HashMap<String, String>,
    cfg: &UpdaterConfig,
    learned_skips: &HashSet<String>,
) -> Vec<UpdateCandidate> {
    let mut out = Vec::new();
    for u in updates {
        if u.name.trim().is_empty() || u.latest.trim().is_empty() {
            continue;
        }
        // Only a genuinely-installed app is a valid native UPDATE target.
        let cur = match match_installed(installed, &u.name) {
            Some(v) => v.clone(),
            None => continue,
        };
        if !is_newer(&u.latest, &cur) {
            continue;
        }
        let id = u.name.to_lowercase();
        if should_skip(cfg, learned_skips, &id) {
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

    fn upd(name: &str, latest: &str) -> AiUpdateRaw {
        AiUpdateRaw {
            name: name.to_string(),
            latest: latest.to_string(),
        }
    }

    #[test]
    fn self_updaters_are_skipped_across_choco_variants() {
        let cfg = UpdaterConfig::default();
        let none = HashSet::new();
        // Both the plain and the .install choco package map to "discord" and skip.
        assert!(should_skip(&cfg, &none, "discord"));
        assert!(should_skip(&cfg, &none, "discord.install"));
        // An unrelated app is not skipped.
        assert!(!should_skip(&cfg, &none, "vscode.install"));
    }

    #[test]
    fn learned_self_updater_is_skipped_across_variants() {
        let cfg = UpdaterConfig::default();
        // A self-updater Eir learned at runtime (keyed by base id) is skipped — including
        // its choco .install variant — even though it isn't in the SELF_UPDATING seed.
        let learned: HashSet<String> = ["spotify".to_string()].into_iter().collect();
        assert!(should_skip(&cfg, &learned, "spotify"));
        assert!(should_skip(&cfg, &learned, "spotify.install"));
        assert!(!should_skip(&cfg, &learned, "vscode"));
    }

    #[test]
    fn user_ignore_matches_base_and_variant() {
        let cfg = UpdaterConfig {
            ignored: vec!["winscp".to_string()],
            ..UpdaterConfig::default()
        };
        let none = HashSet::new();
        // Ignoring the base name also covers the ".install" choco variant.
        assert!(should_skip(&cfg, &none, "winscp"));
        assert!(should_skip(&cfg, &none, "winscp.install"));
        assert!(!should_skip(&cfg, &none, "vscode"));
    }

    #[test]
    fn base_id_strips_only_known_choco_suffixes() {
        assert_eq!(base_id("discord.install"), "discord");
        assert_eq!(base_id("foo.portable"), "foo");
        // A dot that is part of the real name is left alone.
        assert_eq!(base_id("node.js"), "node.js");
        assert_eq!(base_id("paint.net"), "paint.net");
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
            upd("Obsidian", "1.6.0"), // installed + newer -> kept
            upd("Krita", "5.3.0"),    // newer but ignored -> dropped
            upd("OldTool", "2.7.5"),  // installed but older -> dropped
            upd("Empty", ""),         // no latest -> dropped
            upd("GhostApp", "9.0"),   // NOT installed (AI fabrication) -> dropped
        ];
        let cands = native_candidates_from(&updates, &installed, &cfg, &HashSet::new());
        let names: Vec<&str> = cands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Obsidian"]);
        // The kept candidate carries the authoritative installed version + target.
        assert_eq!(cands[0].current, "1.5.0");
        assert_eq!(cands[0].available, "1.6.0");
        assert_eq!(cands[0].methods, vec![Method::Native]);
    }

    #[test]
    fn unmanaged_batch_selects_stalest_first_and_caps() {
        let mut apps = vec![
            ("A".into(), "1".into()),
            ("B".into(), "1".into()),
            ("C".into(), "1".into()),
            ("D".into(), "1".into()),
        ];
        // A checked most recently, B checked recently, C never checked, D checked oldest.
        let last_check: HashMap<String, i64> =
            [("a".into(), 300), ("b".into(), 200), ("d".into(), 100)]
                .into_iter()
                .collect();
        let picked = select_unmanaged_batch(&mut apps, &last_check, 2);
        // Never-checked (C) and oldest-checked (D) come first.
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].0, "C");
        assert_eq!(picked[1].0, "D");
        // The remaining apps are drained from the input vector.
        assert_eq!(apps.len(), 2);
    }
}
