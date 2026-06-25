//! The self-healing loop. For each candidate it tries an update method; on a
//! non-terminal failure the AI diagnostician reads the real captured error and
//! proposes the next method (or an allow-listed remedy), validated by Rust, until one
//! verifies or the methods/attempt cap are exhausted. Rust always classifies the
//! error and has the final say; the AI only proposes within the bounds Rust allows,
//! and a deterministic ladder runs whenever the AI is unavailable.

use crate::ai::client::AiClient;
use crate::updater::config::UpdaterConfig;
use crate::updater::domain::{
    AttemptOutcome, ErrorCategory, Method, NextStep, Remedy, UpdateCandidate, Verification,
};
use crate::updater::methods::{choco, detect, msstore, native, scoop, winget};
use crate::updater::{check, diagnose, history};
use sqlx::SqlitePool;
use std::os::windows::process::CommandExt;
use tracing::warn;

/// CREATE_NO_WINDOW — keep the taskkill console hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Everything an attempt needs that isn't the candidate itself.
pub struct EngineCtx<'a> {
    pub ai: Option<&'a AiClient>,
    pub config: &'a UpdaterConfig,
    /// Model for the web-search calls (the configured `update_check_model`).
    pub model_override: &'a str,
}

/// Kill a process the AI named as holding a lock. The name was already validated to
/// appear in the captured error text; it is further reduced to a safe image-name
/// charset and passed as an argument (never a shell string).
async fn kill_process(name: &str) {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if safe.len() < 3 {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        std::process::Command::new("taskkill")
            .args(["/IM", &safe, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await;
}

/// Run one method against one candidate, first applying any allow-listed remedy the
/// diagnostician requested (kill a locking process, or force the upgrade).
async fn dispatch(
    method: Method,
    remedy: Option<&Remedy>,
    candidate: &UpdateCandidate,
    ctx: &EngineCtx<'_>,
) -> AttemptOutcome {
    if let Some(Remedy::KillProcess { name }) = remedy {
        kill_process(name).await;
    }
    let force = matches!(remedy, Some(Remedy::Force));
    match method {
        Method::Winget => winget::attempt_with(candidate, force).await,
        Method::Choco => choco::attempt(candidate).await,
        Method::Scoop => scoop::attempt(candidate).await,
        Method::MsStore => msstore::attempt(candidate).await,
        Method::Native => match ctx.ai {
            Some(ai) => {
                let note = ctx.config.notes.get(&candidate.id).map(String::as_str);
                let max_bytes = ctx.config.max_installer_mb.saturating_mul(1024 * 1024);
                native::update_native(
                    ai,
                    &candidate.name,
                    &candidate.current,
                    note,
                    ctx.config.native_signature_policy,
                    max_bytes,
                    ctx.model_override,
                )
                .await
            }
            None => AttemptOutcome::failed(
                Method::Native,
                ErrorCategory::NotFound,
                "no AI provider configured for native installs",
            ),
        },
    }
}

/// The deterministic next-method choice: stop on success or a terminal integrity
/// failure, otherwise the first available method not yet tried. Pure — this is the
/// loop core, and Phase 6's AI diagnostician is layered on top of it as a fallback.
fn next_method(order: &[Method], tried: &[Method], last: &AttemptOutcome) -> Option<Method> {
    if last.success {
        return None;
    }
    if last
        .category
        .map(ErrorCategory::is_terminal)
        .unwrap_or(false)
    {
        return None;
    }
    order.iter().copied().find(|m| !tried.contains(m))
}

/// Decide the next step after a non-terminal failure: the AI diagnostician (its
/// proposal validated against the available/untried methods) when a provider is
/// configured, otherwise the deterministic ladder. Returns the step and its AI cost.
async fn decide_next(
    ctx: &EngineCtx<'_>,
    candidate: &UpdateCandidate,
    last: &AttemptOutcome,
    tried: &[Method],
    order: &[Method],
) -> (NextStep, f64) {
    if let Some(ai) = ctx.ai {
        return diagnose::diagnose(ai, ctx.model_override, candidate, last, tried, order).await;
    }
    match next_method(order, tried, last) {
        Some(m) => (NextStep::SwitchTo(m), 0.0),
        None => (
            NextStep::GiveUp("all available methods tried".to_string()),
            0.0,
        ),
    }
}

/// Heal one candidate: try a method, and on a non-terminal failure let the AI read
/// the error and choose the next method (or an allow-listed remedy), repeating until
/// one verifies, an integrity failure is hit, the methods are exhausted, or the
/// attempt cap is reached.
pub async fn heal(
    candidate: &UpdateCandidate,
    ctx: &EngineCtx<'_>,
    available: &[Method],
) -> Vec<AttemptOutcome> {
    let order: Vec<Method> = candidate
        .methods
        .iter()
        .copied()
        .filter(|m| available.contains(m))
        .collect();
    let max = (ctx.config.max_attempts_per_app as usize).max(1);
    let mut attempts: Vec<AttemptOutcome> = Vec::new();
    let mut tried: Vec<Method> = Vec::new();
    let mut current: Option<(Method, Option<Remedy>)> = order.first().map(|&m| (m, None));

    while let Some((method, remedy)) = current.take() {
        if attempts.len() >= max {
            break;
        }
        let mut outcome = dispatch(method, remedy.as_ref(), candidate, ctx).await;
        tried.push(method);

        // Stop on success, a terminal integrity failure, or the last allowed attempt.
        let done = outcome.success
            || outcome
                .category
                .map(ErrorCategory::is_terminal)
                .unwrap_or(false)
            || attempts.len() + 1 >= max;
        if done {
            attempts.push(outcome);
            break;
        }

        let (next, dcost) = decide_next(ctx, candidate, &outcome, &tried, &order).await;
        outcome.cost_usd += dcost;
        attempts.push(outcome);

        current = match next {
            NextStep::SwitchTo(m) => Some((m, None)),
            // We never reboot the machine unattended — defer instead.
            NextStep::RetryWith(_, Remedy::RetryAfterReboot) => None,
            NextStep::RetryWith(m, r) => Some((m, Some(r))),
            NextStep::GiveUp(_) => None,
        };
    }
    attempts
}

/// Methods usable right now: enabled in config AND present on this machine. Missing
/// Chocolatey is bootstrapped when `bootstrap_managers` is set; Scoop is only used if
/// a user already has it (never installed as SYSTEM); native is offered when an AI
/// provider is configured.
pub async fn available_methods(cfg: &UpdaterConfig, ai: Option<&AiClient>) -> Vec<Method> {
    let enabled: Vec<Method> = cfg
        .methods
        .iter()
        .filter_map(|m| Method::from_token(m))
        .collect();
    let has = |m: Method| enabled.contains(&m);
    let mut v = Vec::new();
    if has(Method::Winget) && detect::winget_available() {
        v.push(Method::Winget);
    }
    if has(Method::Choco) {
        let mut present = detect::choco_available();
        if !present && cfg.bootstrap_managers {
            present = detect::bootstrap_choco().await;
        }
        if present {
            v.push(Method::Choco);
        }
    }
    if has(Method::Scoop) && detect::scoop_available() {
        v.push(Method::Scoop);
    }
    if has(Method::MsStore) && detect::winget_available() {
        v.push(Method::MsStore);
    }
    if cfg.native_enabled && ai.is_some() {
        v.push(Method::Native);
    }
    v
}

/// The outcome of one full update cycle.
pub struct CycleSummary {
    pub results: Vec<(UpdateCandidate, Vec<AttemptOutcome>)>,
    pub notes: Vec<String>,
    pub cost_usd: f64,
}

/// Flatten a cycle's per-candidate attempts into one UI row each: the winning attempt
/// (the verified/installed one if any, else the last tried) decides the row's state.
pub fn app_rows(summary: &CycleSummary) -> Vec<eir_proto::UpdaterAppRow> {
    summary
        .results
        .iter()
        .map(|(cand, outcomes)| {
            let winner = outcomes
                .iter()
                .find(|o| o.success)
                .or_else(|| outcomes.last());
            let (method, state, detail, signature, to) = match winner {
                None => (
                    String::new(),
                    "skipped".to_string(),
                    "no available method".to_string(),
                    String::new(),
                    String::new(),
                ),
                Some(o) => {
                    let state = if o.success {
                        if o.verification == Verification::Verified {
                            "verified"
                        } else {
                            "installed"
                        }
                    } else {
                        "failed"
                    };
                    (
                        o.method.as_str().to_string(),
                        state.to_string(),
                        o.detail.clone(),
                        o.signature.clone().unwrap_or_default(),
                        o.installed_version
                            .clone()
                            .unwrap_or_else(|| cand.available.clone()),
                    )
                }
            };
            eir_proto::UpdaterAppRow {
                id: cand.id.clone(),
                name: cand.name.clone(),
                from: cand.current.clone(),
                to,
                method,
                state,
                detail,
                signature,
            }
        })
        .collect()
}

/// Run one full cycle: check for candidates, heal each (bounded by the per-run app
/// and budget caps), and persist every attempt. The cycle id groups this run's
/// attempts in the history table.
pub async fn run_cycle(pool: &SqlitePool, ctx: &EngineCtx<'_>, cycle_id: i64) -> CycleSummary {
    let available = available_methods(ctx.config, ctx.ai).await;
    let check = check::collect(ctx.ai, ctx.config, ctx.model_override, &available).await;
    let budget = ctx.config.budget_usd_per_run;
    let mut spent = check.cost_usd;
    let mut notes = check.notes;
    let mut results = Vec::new();

    for cand in check
        .candidates
        .into_iter()
        .take(ctx.config.max_apps_per_run as usize)
    {
        if budget > 0.0 && spent >= budget {
            notes.push(format!(
                "Stopped at the £/$ budget after {} apps.",
                results.len()
            ));
            break;
        }
        let outcomes = heal(&cand, ctx, &available).await;
        spent += outcomes.iter().map(|o| o.cost_usd).sum::<f64>();
        if let Err(e) = history::record_attempts(pool, cycle_id, &cand, &outcomes).await {
            warn!("failed to record update history for {}: {e}", cand.name);
        }
        results.push((cand, outcomes));
    }

    CycleSummary {
        results,
        notes,
        cost_usd: spent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::domain::Verification;

    fn outcome(method: Method, success: bool, category: Option<ErrorCategory>) -> AttemptOutcome {
        AttemptOutcome {
            method,
            success,
            verification: if success {
                Verification::Verified
            } else {
                Verification::NotChecked
            },
            category,
            exit_code: None,
            installed_version: None,
            detail: String::new(),
            signature: None,
            sha256: None,
            cost_usd: 0.0,
        }
    }

    #[test]
    fn next_method_stops_on_success() {
        let order = [Method::Winget, Method::Native];
        let last = outcome(Method::Winget, true, None);
        assert_eq!(next_method(&order, &[Method::Winget], &last), None);
    }

    #[test]
    fn next_method_stops_on_terminal_integrity_failure() {
        let order = [Method::Winget, Method::Native];
        let last = outcome(
            Method::Native,
            false,
            Some(ErrorCategory::SignatureRejected),
        );
        // Even though winget is untried, a terminal integrity failure ends it.
        assert_eq!(next_method(&order, &[Method::Native], &last), None);
    }

    #[test]
    fn next_method_advances_to_the_next_untried_method() {
        let order = [Method::Winget, Method::Native];
        let last = outcome(Method::Winget, false, Some(ErrorCategory::InstallerFailed));
        assert_eq!(
            next_method(&order, &[Method::Winget], &last),
            Some(Method::Native)
        );
        // Once both are tried, there's nowhere left to go.
        let last2 = outcome(Method::Native, false, Some(ErrorCategory::InstallerFailed));
        assert_eq!(
            next_method(&order, &[Method::Winget, Method::Native], &last2),
            None
        );
    }
}
