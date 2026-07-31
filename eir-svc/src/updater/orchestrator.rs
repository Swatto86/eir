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
use crate::updater::verify::{verify_app, VerifyTarget};
use crate::updater::{check, diagnose, history};
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

/// Run one method against one candidate, first applying any allow-listed remedy the
/// diagnostician requested (kill a locking process, or force the upgrade).
async fn dispatch(
    method: Method,
    remedy: Option<&Remedy>,
    candidate: &UpdateCandidate,
    ctx: &EngineCtx<'_>,
) -> AttemptOutcome {
    if let Some(Remedy::KillProcess { name }) = remedy {
        if let Err(error) = crate::executor::process::kill(name).await {
            return AttemptOutcome::failed(
                method,
                ErrorCategory::LockHeld,
                format!("Lock-holder remedy was refused or failed: {error}"),
            );
        }
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
                let max_bytes = ctx.config.max_installer_mb.saturating_mul(1024 * 1024);
                native::update_native(
                    ai,
                    candidate,
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
fn stops_healing(category: Option<ErrorCategory>) -> bool {
    category
        .map(|category| {
            category.is_terminal()
                || matches!(
                    category,
                    ErrorCategory::AlreadyCurrent | ErrorCategory::NeedsReboot
                )
        })
        .unwrap_or(false)
}

fn next_method(order: &[Method], tried: &[Method], last: &AttemptOutcome) -> Option<Method> {
    if last.success {
        return None;
    }
    // A terminal integrity failure, or "already current" (the app is up to date, so
    // switching to another method would needlessly reinstall it), or a pending reboot
    // stops the ladder.
    if stops_healing(last.category) {
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
    let mut current: Option<(Method, Option<Remedy>)> = order.first().map(|&m| (m, None));

    while let Some((method, remedy)) = current.take() {
        if attempts.len() >= max {
            break;
        }
        let mut outcome = dispatch(method, remedy.as_ref(), candidate, ctx).await;
        tried.push(method);

        // Stop on success, a terminal integrity failure, "already current" (no work
        // to do), a pending reboot, or the last allowed attempt.
        let done = outcome.success || stops_healing(outcome.category) || attempts.len() + 1 >= max;
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
/// Chocolatey is bootstrapped when `bootstrap_managers` is set; Scoop remains
/// unavailable because its user-owned shim cannot run safely as SYSTEM; native is
/// offered when an AI provider is configured.
pub async fn available_methods(cfg: &UpdaterConfig, ai: Option<&AiClient>) -> Vec<Method> {
    let enabled: Vec<Method> = cfg
        .methods
        .iter()
        .filter_map(|m| Method::from_token(m))
        .collect();
    let has = |m: Method| enabled.contains(&m);
    let mut v = Vec::new();
    let winget_present = if has(Method::Winget) || has(Method::MsStore) {
        detect::winget_available_async().await
    } else {
        false
    };
    if has(Method::Winget) && winget_present {
        v.push(Method::Winget);
    }
    if has(Method::Choco) {
        let mut present = detect::choco_available_async().await;
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
    if has(Method::MsStore) && winget_present {
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
    /// Completion time for every cycle, including failed/empty runs.
    pub completed_at: i64,
    /// Persisted completion time for a cycle with no check/app failures (0 otherwise).
    pub clean_at: i64,
    /// A targeted retry updates one prior row and must not postpone the full schedule.
    pub targeted: bool,
    /// Concise guidance proven by an observed successful targeted install.
    pub learned_guidance: Option<GuidanceLearning>,
}

pub struct GuidanceLearning {
    pub id: String,
    pub note: String,
}

fn app_completed_cleanly(outcomes: &[AttemptOutcome]) -> bool {
    outcomes.iter().any(|outcome| outcome.success)
        || outcomes
            .last()
            .is_some_and(|outcome| outcome.category == Some(ErrorCategory::AlreadyCurrent))
}

fn cycle_completed_cleanly<'a>(
    had_errors: bool,
    deferred: usize,
    mut outcomes: impl Iterator<Item = &'a [AttemptOutcome]>,
) -> bool {
    !had_errors && deferred == 0 && outcomes.all(app_completed_cleanly)
}

/// Flatten a cycle's per-candidate attempts into one UI row each: the winning attempt
/// (the verified/installed one if any, else the last tried) decides the row's state.
pub fn app_rows(summary: &CycleSummary, config: &UpdaterConfig) -> Vec<eir_proto::UpdaterAppRow> {
    let mut rows: Vec<_> = summary
        .results
        .iter()
        .map(|(cand, outcomes)| {
            let winner = outcomes
                .iter()
                .find(|o| o.success)
                .or_else(|| outcomes.last());
            let (method, state, detail, signature, from, to) = match winner {
                None => (
                    String::new(),
                    "skipped".to_string(),
                    "no available method".to_string(),
                    String::new(),
                    cand.current.clone(),
                    String::new(),
                ),
                Some(o) => {
                    let already_current = o.category == Some(ErrorCategory::AlreadyCurrent);
                    let state = if already_current {
                        "current"
                    } else if o.success {
                        if o.verification == Verification::Verified {
                            "verified"
                        } else {
                            "installed"
                        }
                    } else {
                        "failed"
                    };
                    let from = if already_current {
                        o.installed_version
                            .clone()
                            .filter(|version| !version.is_empty())
                            .unwrap_or_else(|| cand.current.clone())
                    } else {
                        cand.current.clone()
                    };
                    (
                        o.method.as_str().to_string(),
                        state.to_string(),
                        o.detail.clone(),
                        o.signature.clone().unwrap_or_default(),
                        from.clone(),
                        if already_current {
                            from
                        } else {
                            o.installed_version
                                .clone()
                                .unwrap_or_else(|| cand.available.clone())
                        },
                    )
                }
            };
            eir_proto::UpdaterAppRow {
                id: cand.id.clone(),
                name: cand.name.clone(),
                from,
                to,
                method,
                state,
                detail,
                signature,
                ignored: false,
                note: config.notes.get(&cand.id).cloned().unwrap_or_default(),
            }
        })
        .collect();
    if rows.is_empty() && summary.clean_at == 0 {
        rows.push(warning_row(
            "Check finished with warnings; review the notes below.",
        ));
    }
    rows
}

pub fn warning_row(detail: &str) -> eir_proto::UpdaterAppRow {
    eir_proto::UpdaterAppRow {
        id: "update-check".to_string(),
        name: "Update check".to_string(),
        state: "failed".to_string(),
        detail: detail.to_string(),
        ..Default::default()
    }
}

/// Run one full cycle: check for candidates, heal each (bounded by the per-run app
/// cap), and persist every attempt. The cycle id groups this run's attempts in the
/// history table.
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
        None,
    )
    .await;
    let mut spent = check.cost_usd;
    let mut notes = check.notes;
    let mut had_errors = check.had_errors;
    if available.is_empty() {
        had_errors = true;
        notes.push("No usable update source is available.".to_string());
    }
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
        let _ = progress.send(format!("updating {}…", cand.name)).await;
        let outcomes = heal(&cand, ctx, &available, &learned).await;
        spent += outcomes.iter().map(|o| o.cost_usd).sum::<f64>();
        if let Err(e) = history::record_attempts(pool, cycle_id, &cand, &outcomes).await {
            warn!("failed to record update history for {}: {e}", cand.name);
            notes.push(format!(
                "failed to record update history for {}: {e}",
                cand.name
            ));
            had_errors = true;
        }
        results.push((cand, outcomes));
    }

    // Learn from this cycle's (and recent) attempts: an app that keeps timing out with no
    // success is a self-updater to stop fighting; a method that keeps failing while another
    // works is deprioritised. Best-effort, on already-collected data — no AI, no extra I/O.
    crate::learn::analyse_updates(pool).await;

    let completed_at = chrono::Utc::now().timestamp();
    let mut clean_at = 0;
    if cycle_completed_cleanly(
        had_errors,
        deferred,
        results.iter().map(|(_, outcomes)| outcomes.as_slice()),
    ) {
        match history::record_clean_run(pool, completed_at).await {
            Ok(()) => clean_at = completed_at,
            Err(e) => notes.push(format!("failed to persist clean update cycle: {e}")),
        }
    }

    CycleSummary {
        results,
        notes,
        cost_usd: spent,
        completed_at,
        clean_at,
        targeted: false,
        learned_guidance: None,
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn guidance_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|word| word.starts_with("https://") || word.starts_with("http://"))
        .map(|word| {
            word.trim_end_matches(['.', ',', ';', ')', ']', '}'])
                .to_string()
        })
        .collect()
}

/// Accept a concise AI rewrite only when it preserves every explicit URL. A small
/// length increase is allowed so broken English can be corrected without losing facts.
fn concise_guidance(original: &str, proposed: &str) -> String {
    let original = one_line(original);
    let proposed = one_line(proposed.trim().trim_matches(['`', '"']));
    let original_chars = original.chars().count();
    let rewrite_limit = original_chars
        .saturating_add((original_chars / 4).max(40))
        .min(crate::updater::config::MAX_APP_NOTE_CHARS);
    let original_urls = guidance_urls(&original);
    let proposed_urls = guidance_urls(&proposed);
    let keeps_urls = original_urls.iter().all(|url| proposed.contains(url))
        && proposed_urls.iter().all(|url| original_urls.contains(url));
    if !proposed.is_empty() && proposed.chars().count() <= rewrite_limit && keeps_urls {
        proposed
    } else {
        original
    }
}

fn refinement_prompt(
    candidate: &UpdateCandidate,
    outcome: &AttemptOutcome,
    guidance: &str,
) -> String {
    format!(
        "Rewrite the following guidance as one concise instruction in clear, precise, \
grammatically correct English for future Windows software updates. Preserve the exact product \
identity, every URL, archive member, installer flag, and other fact needed for the successful \
method. Do not mention this retry or claim anything not shown. Return only the instruction, no \
quotes or markdown.\n\n\
Product: {}\nSuccessful method: {}\nResult: {}\nGuidance: {}",
        candidate.name,
        outcome.method.as_str(),
        outcome.detail,
        guidance
    )
}

fn failure_explanation_prompt(
    candidate: &UpdateCandidate,
    guidance: &str,
    outcomes: &[AttemptOutcome],
) -> String {
    let attempts = outcomes
        .iter()
        .map(|outcome| {
            format!(
                "- {} / {:?}: {}",
                outcome.method.as_str(),
                outcome.category.unwrap_or(ErrorCategory::Unknown),
                outcome.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Explain in one concise sentence, using clear, precise, grammatically correct English, why \
this Windows software update still failed after the saved guidance was applied. Use only the \
recorded facts below. Do not invent a cause, suggest bypassing an integrity check, or repeat the \
guidance verbatim. Return only the sentence, no quotes or markdown.\n\n\
Product: {} (installed {}, target {})\nSaved guidance: {}\nRecorded attempts:\n{}",
        candidate.name, candidate.current, candidate.available, guidance, attempts
    )
}

async fn refine_guidance(
    ctx: &EngineCtx<'_>,
    candidate: &UpdateCandidate,
    outcome: &AttemptOutcome,
    guidance: &str,
) -> (String, f64) {
    let Some(ai) = ctx.ai else {
        return (one_line(guidance), 0.0);
    };
    let prompt = refinement_prompt(candidate, outcome, guidance);
    match ai.complete_text(&prompt, ctx.model_override).await {
        Ok((text, usage)) => (
            concise_guidance(guidance, &text),
            usage.map(|value| value.cost_usd).unwrap_or(0.0),
        ),
        Err(error) => {
            warn!("Could not refine successful update guidance: {error}");
            (one_line(guidance), 0.0)
        }
    }
}

async fn explain_retry_failure(
    ctx: &EngineCtx<'_>,
    candidate: &UpdateCandidate,
    outcomes: &[AttemptOutcome],
    guidance: &str,
) -> (Option<String>, f64) {
    let Some(ai) = ctx.ai else {
        return (None, 0.0);
    };
    let prompt = failure_explanation_prompt(candidate, guidance, outcomes);
    match ai.complete_text(&prompt, ctx.model_override).await {
        Ok((text, usage)) => {
            let explanation: String = one_line(text.trim().trim_matches(['`', '"']))
                .chars()
                .take(500)
                .collect();
            (
                (!explanation.is_empty()).then_some(explanation),
                usage.map(|value| value.cost_usd).unwrap_or(0.0),
            )
        }
        Err(error) => {
            warn!("Could not explain guided update failure: {error}");
            (None, 0.0)
        }
    }
}

fn guided_failure_needs_explanation(
    candidate: &UpdateCandidate,
    outcomes: &[AttemptOutcome],
) -> bool {
    candidate.guidance.is_some()
        && outcomes.iter().all(|outcome| !outcome.success)
        && outcomes.last().and_then(|outcome| outcome.category)
            != Some(ErrorCategory::AlreadyCurrent)
}

/// Re-check and retry exactly one previously failed app. The targeted check uses the
/// latest guidance, bypasses detector skips because this is an explicit user action,
/// and never advances the full-cycle schedule.
pub async fn run_retry(
    pool: &SqlitePool,
    ctx: &EngineCtx<'_>,
    cycle_id: i64,
    progress: &ProgressTx,
    prior: eir_proto::UpdaterAppRow,
) -> CycleSummary {
    let _ = progress.send(format!("re-checking {}…", prior.name)).await;
    let mut retry_config = ctx.config.clone();
    retry_config
        .ignored
        .retain(|id| !id.eq_ignore_ascii_case(&prior.id));
    let retry_ctx = EngineCtx {
        ai: ctx.ai,
        config: &retry_config,
        model_override: ctx.model_override,
    };
    let available = available_methods(retry_ctx.config, retry_ctx.ai).await;
    let no_learned_skips = std::collections::HashSet::new();
    let check = check::collect(
        pool,
        retry_ctx.ai,
        retry_ctx.config,
        retry_ctx.model_override,
        &available,
        &no_learned_skips,
        Some(&prior.id),
    )
    .await;
    let mut cost_usd = check.cost_usd;
    let notes = check.notes;
    let check_had_errors = check.had_errors || available.is_empty();
    let learned = crate::learn::LearnedFacts::load(pool).await;
    let mut learned_guidance = None;

    let (candidate, mut outcomes) = if let Some(candidate) = check.candidates.into_iter().next() {
        let _ = progress.send(format!("retrying {}…", candidate.name)).await;
        let guidance = candidate.guidance.clone();
        let mut outcomes = heal(&candidate, &retry_ctx, &available, &learned).await;
        if let Some(success) = outcomes.iter_mut().find(|outcome| outcome.success) {
            if guidance.is_some() {
                success.detail = format!("Updated successfully using guidance. {}", success.detail);
            }
            if let Some(guidance) = guidance {
                let (note, refinement_cost) =
                    refine_guidance(&retry_ctx, &candidate, success, &guidance).await;
                cost_usd += refinement_cost;
                learned_guidance = Some(GuidanceLearning {
                    id: candidate.id.clone(),
                    note,
                });
            }
        }
        (candidate, outcomes)
    } else {
        let method = Method::from_token(&prior.method)
            .or_else(|| available.first().copied())
            .unwrap_or(Method::Native);
        let guidance = retry_ctx.config.guidance_for(&prior.id).map(str::to_string);
        let observed_version = if check_had_errors {
            String::new()
        } else {
            verify_app(
                &VerifyTarget::ByName {
                    name: prior.name.clone(),
                },
                &prior.to,
            )
            .await
            .1
        };
        let candidate = UpdateCandidate {
            id: prior.id,
            name: prior.name,
            current: prior.from,
            available: prior.to,
            package_id: None,
            guidance: guidance.clone(),
            methods: vec![method],
        };
        let mut outcome = if check_had_errors {
            let reason = notes.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
            AttemptOutcome::failed(
                method,
                ErrorCategory::Unknown,
                format!(
                    "Re-check could not confirm an update: {}",
                    if reason.is_empty() {
                        "the update sources returned no usable result"
                    } else {
                        &reason
                    }
                ),
            )
        } else {
            AttemptOutcome::failed(
                method,
                ErrorCategory::AlreadyCurrent,
                if guidance.is_some() {
                    "Re-check completed with the saved guidance; no newer update is currently available."
                } else {
                    "Re-check completed; no newer update is currently available."
                },
            )
        };
        if !observed_version.is_empty() {
            outcome.installed_version = Some(observed_version);
        }
        (candidate, vec![outcome])
    };

    if guided_failure_needs_explanation(&candidate, &outcomes) {
        let (explanation, explanation_cost) = explain_retry_failure(
            &retry_ctx,
            &candidate,
            &outcomes,
            candidate.guidance.as_deref().unwrap_or_default(),
        )
        .await;
        cost_usd += explanation_cost;
        if let Some(failure) = outcomes.last_mut() {
            failure.detail = format!(
                "Update still failed after applying guidance: {}{}",
                failure.detail,
                explanation
                    .map(|text| format!(" AI explanation: {text}"))
                    .unwrap_or_default()
            );
        }
    }

    cost_usd += outcomes.iter().map(|outcome| outcome.cost_usd).sum::<f64>();
    if let Err(error) = history::record_attempts(pool, cycle_id, &candidate, &outcomes).await {
        warn!(
            "failed to record targeted update history for {}: {error}",
            candidate.name
        );
    }
    crate::learn::analyse_updates(pool).await;

    CycleSummary {
        results: vec![(candidate, outcomes)],
        notes,
        cost_usd,
        completed_at: chrono::Utc::now().timestamp(),
        clean_at: 0,
        targeted: true,
        learned_guidance,
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
    fn next_method_stops_when_a_reboot_is_required() {
        let order = [Method::Winget, Method::Choco];
        let last = outcome(Method::Winget, false, Some(ErrorCategory::NeedsReboot));
        assert_eq!(next_method(&order, &[Method::Winget], &last), None);
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

    #[test]
    fn clean_completion_accepts_success_or_already_current_only() {
        assert!(app_completed_cleanly(&[outcome(
            Method::Winget,
            true,
            None
        )]));
        assert!(app_completed_cleanly(&[outcome(
            Method::Winget,
            false,
            Some(ErrorCategory::AlreadyCurrent)
        )]));
        assert!(!app_completed_cleanly(&[]));
        assert!(!app_completed_cleanly(&[outcome(
            Method::Winget,
            false,
            Some(ErrorCategory::InstallerFailed)
        )]));
    }

    #[test]
    fn deferred_apps_prevent_a_clean_cycle() {
        let successful = vec![outcome(Method::Winget, true, None)];
        assert!(cycle_completed_cleanly(
            false,
            0,
            std::iter::once(successful.as_slice())
        ));
        assert!(!cycle_completed_cleanly(
            false,
            1,
            std::iter::once(successful.as_slice())
        ));
    }

    #[test]
    fn already_current_rows_are_not_reported_as_failures() {
        let candidate = UpdateCandidate {
            id: "tool".into(),
            name: "Tool".into(),
            current: "1.9".into(),
            available: "2.0".into(),
            package_id: None,
            guidance: None,
            methods: vec![Method::Winget],
        };
        let mut current = outcome(Method::Winget, false, Some(ErrorCategory::AlreadyCurrent));
        current.installed_version = Some("2.0".into());
        let summary = CycleSummary {
            results: vec![(candidate, vec![current])],
            notes: vec![],
            cost_usd: 0.0,
            completed_at: 1,
            clean_at: 1,
            targeted: false,
            learned_guidance: None,
        };
        let row = &app_rows(&summary, &UpdaterConfig::default())[0];
        assert_eq!(row.state, "current");
        assert_eq!(row.from, "2.0");
        assert_eq!(row.to, "2.0");
    }

    #[test]
    fn incomplete_empty_cycle_has_visible_warning_row() {
        let summary = CycleSummary {
            results: vec![],
            notes: vec!["inventory failed".into()],
            cost_usd: 0.0,
            completed_at: 1,
            clean_at: 0,
            targeted: false,
            learned_guidance: None,
        };
        let rows = app_rows(&summary, &UpdaterConfig::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "failed");
        assert_eq!(rows[0].name, "Update check");
    }

    #[test]
    fn learned_guidance_is_single_line_and_never_drops_urls() {
        let original =
            "Use the x64 archive.\nOfficial releases: https://example.com/releases/latest";
        assert_eq!(
            concise_guidance(original, "Use x64 from https://example.com/releases/latest"),
            "Use x64 from https://example.com/releases/latest"
        );
        assert_eq!(
            concise_guidance(original, "Use the vendor's x64 archive"),
            "Use the x64 archive. Official releases: https://example.com/releases/latest"
        );
        assert_eq!(
            concise_guidance(
                original,
                "Use https://example.com/releases/latest or https://evil.example/update"
            ),
            "Use the x64 archive. Official releases: https://example.com/releases/latest"
        );
        assert_eq!(
            concise_guidance(
                "Get x64 here: https://example.com/releases/latest",
                "Download the latest x64 Windows installer from https://example.com/releases/latest"
            ),
            "Download the latest x64 Windows installer from https://example.com/releases/latest"
        );
    }

    #[test]
    fn guided_retry_prompts_require_precise_english_and_real_failure_evidence() {
        let candidate = UpdateCandidate {
            id: "tool".into(),
            name: "Tool".into(),
            current: "1.0".into(),
            available: "2.0".into(),
            package_id: None,
            guidance: Some("Use https://example.com/releases/latest".into()),
            methods: vec![Method::Native],
        };
        let failed = AttemptOutcome::failed(
            Method::Native,
            ErrorCategory::InstallerFailed,
            "installer exited with code 1603",
        );
        let explanation = failure_explanation_prompt(
            &candidate,
            candidate.guidance.as_deref().unwrap(),
            std::slice::from_ref(&failed),
        );
        assert!(explanation.contains("precise, grammatically correct English"));
        assert!(explanation.contains("installer exited with code 1603"));
        assert!(explanation.contains("https://example.com/releases/latest"));

        let refinement =
            refinement_prompt(&candidate, &failed, candidate.guidance.as_deref().unwrap());
        assert!(refinement.contains("precise, grammatically correct English"));

        assert!(guided_failure_needs_explanation(&candidate, &[failed]));
        assert!(!guided_failure_needs_explanation(
            &candidate,
            &[AttemptOutcome::failed(
                Method::Native,
                ErrorCategory::AlreadyCurrent,
                "No newer update is available."
            )]
        ));
    }
}
