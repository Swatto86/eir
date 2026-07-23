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
use crate::updater::{check, diagnose, history, proc};
use sqlx::SqlitePool;
use tracing::warn;

/// Coarse progress messages a running cycle emits so the UI's phase label tracks
/// reality ("checking…" → "updating {app}…") instead of freezing on the start label.
/// A closed/full receiver is ignored — progress is best-effort and never blocks work.
pub type ProgressTx = tokio::sync::mpsc::Sender<String>;

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
    let _ = proc::run_capped(
        "taskkill",
        &["/IM".to_string(), safe, "/F".to_string()],
        proc::PROBE,
    )
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
    // A stale manager lock is almost always transient (a prior operation releasing).
    // We can't safely delete a package manager's lock file, and killing the holder is
    // the KillProcess remedy's job — so give the lock a moment to clear, then retry
    // with force where the method supports it. Previously this remedy was validated
    // and accepted but never applied, silently burning one capped attempt.
    if matches!(remedy, Some(Remedy::ClearManagerLock)) {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let force = matches!(remedy, Some(Remedy::Force))
        || (matches!(remedy, Some(Remedy::ClearManagerLock)) && method.supports_force());
    match method {
        Method::Winget => winget::attempt_with(candidate, force).await,
        Method::Choco => choco::attempt_with(candidate, force).await,
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
    // A terminal integrity failure, or "already current" (the app is up to date, so
    // switching to another method would needlessly reinstall it), stops the ladder.
    if last
        .category
        .map(|c| c.is_terminal() || c == ErrorCategory::AlreadyCurrent)
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
    budget_remaining: f64,
    learned: &crate::learn::LearnedFacts,
) -> Vec<AttemptOutcome> {
    let mut order: Vec<Method> = candidate
        .methods
        .iter()
        .copied()
        .filter(|m| available.contains(m))
        .collect();
    // A method the machine has learned keeps failing for this app is pushed to the back
    // (still a fallback, never removed). Stable sort preserves the configured order
    // within each group; `false` (not deprioritised) sorts before `true`.
    let app_base = crate::updater::check::base_id(&candidate.id);
    order.sort_by_key(|m| learned.is_method_deprioritised(app_base, m.as_str()));
    let max = (ctx.config.max_attempts_per_app as usize).max(1);
    let mut attempts: Vec<AttemptOutcome> = Vec::new();
    let mut tried: Vec<Method> = Vec::new();
    // AI spend within THIS app's heal, so the per-run budget is a true ceiling and
    // not just a between-apps gate.
    let mut app_spent = 0.0_f64;
    let mut current: Option<(Method, Option<Remedy>)> = order.first().map(|&m| (m, None));

    while let Some((method, remedy)) = current.take() {
        if attempts.len() >= max {
            break;
        }
        // Never START a paid native install once the run's AI budget is spent.
        if method == Method::Native && app_spent >= budget_remaining {
            break;
        }
        let mut outcome = dispatch(method, remedy.as_ref(), candidate, ctx).await;
        app_spent += outcome.cost_usd;
        tried.push(method);

        // Stop on success, a terminal integrity failure, "already current" (no work
        // to do), or the last allowed attempt.
        let done = outcome.success
            || outcome
                .category
                .map(|c| c.is_terminal() || c == ErrorCategory::AlreadyCurrent)
                .unwrap_or(false)
            || attempts.len() + 1 >= max;
        if done {
            attempts.push(outcome);
            break;
        }

        // Pay for AI diagnosis only while within budget; otherwise take the free
        // deterministic next step.
        let (next, dcost) = if app_spent < budget_remaining {
            decide_next(ctx, candidate, &outcome, &tried, &order).await
        } else {
            let det = next_method(&order, &tried, &outcome)
                .map(NextStep::SwitchTo)
                .unwrap_or_else(|| NextStep::GiveUp("AI budget reached".to_string()));
            (det, 0.0)
        };
        app_spent += dcost;
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
                ignored: false,
            }
        })
        .collect()
}

/// Run one full cycle: check for candidates, heal each (bounded by the per-run app
/// and budget caps), and persist every attempt. The cycle id groups this run's
/// attempts in the history table.
pub async fn run_cycle(
    pool: &SqlitePool,
    ctx: &EngineCtx<'_>,
    cycle_id: i64,
    progress: &ProgressTx,
) -> CycleSummary {
    let _ = progress.send("checking…".to_string()).await;
    let available = available_methods(ctx.config, ctx.ai).await;
    // Self-updaters Eir has learned to leave alone (e.g. an app whose package-manager
    // update keeps timing out) are skipped during candidate collection — see learn::.
    let learned_skips = crate::learn::active_self_updater_subjects(pool)
        .await
        .unwrap_or_default();
    let learned = crate::learn::LearnedFacts::load(pool).await;
    let check = check::collect(
        pool,
        ctx.ai,
        ctx.config,
        ctx.model_override,
        &available,
        &learned_skips,
    )
    .await;
    let budget = ctx.config.budget_usd_per_run;
    let mut spent = check.cost_usd;
    let mut notes = check.notes;
    let mut results = Vec::new();

    // Fair rotation: order candidates by how recently each was last attempted (never-tried
    // first, then stalest). The candidate collection order is otherwise fixed, so without
    // this a persistently-failing app at the front would permanently starve every app past
    // `max_apps_per_run`. A stable sort keeps the collection order for equal staleness.
    let last_attempt = crate::updater::history::last_attempt_times(pool)
        .await
        .unwrap_or_default();
    let mut candidates = check.candidates;
    candidates.sort_by_key(|c| last_attempt.get(&c.id).copied().unwrap_or(0));
    let cap = ctx.config.max_apps_per_run as usize;
    let deferred = candidates.len().saturating_sub(cap);
    if deferred > 0 {
        notes.push(format!(
            "{deferred} more app(s) past the per-run cap of {cap} — deferred to a later cycle (stalest first)."
        ));
    }

    for cand in candidates.into_iter().take(cap) {
        if budget > 0.0 && spent >= budget {
            notes.push(format!(
                "Stopped at the £/$ budget after {} apps.",
                results.len()
            ));
            break;
        }
        let remaining = if budget > 0.0 {
            (budget - spent).max(0.0)
        } else {
            f64::INFINITY
        };
        let _ = progress.send(format!("updating {}…", cand.name)).await;
        let outcomes = heal(&cand, ctx, &available, remaining, &learned).await;
        spent += outcomes.iter().map(|o| o.cost_usd).sum::<f64>();
        if let Err(e) = history::record_attempts(pool, cycle_id, &cand, &outcomes).await {
            warn!("failed to record update history for {}: {e}", cand.name);
        }
        results.push((cand, outcomes));
    }

    // Learn from this cycle's (and recent) attempts: an app that keeps timing out with no
    // success is a self-updater to stop fighting; a method that keeps failing while another
    // works is deprioritised. Best-effort, on already-collected data — no AI, no extra I/O.
    crate::learn::analyse_updates(pool).await;

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
