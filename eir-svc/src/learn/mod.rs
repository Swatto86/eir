//! Machine-pattern self-improvement.
//!
//! Eir learns patterns about the specific machine from its own audit history and adapts
//! — deterministically, with no AI in the write-path, and only ever toward *more*
//! conservative behaviour (skip, deprioritise a method, lower confidence). See
//! ARCHITECTURE.md "Self-improvement" for the full design.
//!
//! Facts are derived each cycle from the audit tables, persisted to `learned_facts`
//! (migration 0008), and applied at existing decision seams. Detection runs in two
//! places on already-collected data (no AI, no extra external I/O):
//! [`analyse_updates`] in the updater cycle (self-updaters, failing methods) and
//! [`analyse_issues`] in the decision loop (ineffective fixes, user-rejected actions).

mod apply;
mod detect;
mod label;
mod store;

pub use apply::LearnedFacts;
pub use label::label_one;
pub use store::{
    active_self_updater_subjects, clear_detector_facts, facts_for_view, record_rejection,
    set_learned_fact,
};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{info, warn};

/// Rolling window (days) over which evidence is counted for every detector.
const WINDOW_DAYS: i64 = 30;
/// Supporting observations required before a fact forms.
const QUORUM: i64 = 3;
/// Largest confidence haircut a learned fact may apply (never lets learning block a
/// fix outright — only nudges it below the threshold the user already set).
pub const MAX_CONFIDENCE_PENALTY: f32 = 0.15;

/// The kinds of learned fact. The token is what `learned_facts.kind` stores;
/// `from_token` returns None on an unknown token so a row from a newer build is skipped,
/// never blindly trusted (same defensive pattern as `Method::from_token`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnedFactKind {
    /// An app whose package-manager update keeps timing out — stop managing it.
    SelfUpdaterSuspected,
    /// A method that keeps failing for an app while another method works — try it last.
    MethodFailing,
    /// A fix action type that "succeeds" but never improves the system — trust it less.
    FixIneffective,
    /// An action the user keeps rejecting — propose it less readily.
    RejectedSignal,
}

impl LearnedFactKind {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::SelfUpdaterSuspected => "self_updater_suspected",
            Self::MethodFailing => "method_failing",
            Self::FixIneffective => "fix_ineffective",
            Self::RejectedSignal => "rejected_signal",
        }
    }

    /// None on an unknown token — a row written by a newer build is skipped, never trusted.
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "self_updater_suspected" => Some(Self::SelfUpdaterSuspected),
            "method_failing" => Some(Self::MethodFailing),
            "fix_ineffective" => Some(Self::FixIneffective),
            "rejected_signal" => Some(Self::RejectedSignal),
            _ => None,
        }
    }
}

/// The behavioural effect of a learned fact — a CLOSED, conservative-only set. There is
/// deliberately no variant that enables an action, raises confidence, unblocks a target,
/// or adds a method: the worst a wrong fact can do is make Eir do *less*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Effect {
    /// Don't try to update this app at all.
    Skip,
    /// Move this method to the end of the order for this app (still a fallback).
    DeprioritiseMethod { method: String },
    /// Subtract this much (capped) from a proposed action's confidence before gating.
    ConfidencePenalty { amount: f32 },
}

impl Effect {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"type\":\"skip\"}".to_string())
    }
}

/// Updater-cycle detection: learn self-updaters and failing methods from `update_attempts`.
/// Best-effort — a failure is logged and never disturbs the cycle.
pub async fn analyse_updates(pool: &SqlitePool) {
    let rows = match store::update_attempt_rows(pool, WINDOW_DAYS).await {
        Ok(r) => r,
        Err(e) => {
            warn!("self-improvement: reading update history failed: {e}");
            return;
        }
    };

    // Self-updaters → Skip.
    let self_updaters = detect::detect_self_updaters(&rows, QUORUM);
    let su_subjects: std::collections::HashSet<String> =
        self_updaters.iter().map(|c| c.subject.clone()).collect();
    for c in &self_updaters {
        let evidence = format!(
            "{} timed-out update cycles, 0 successes in {WINDOW_DAYS}d",
            c.timeout_cycles
        );
        upsert(
            pool,
            LearnedFactKind::SelfUpdaterSuspected,
            &c.subject,
            &Effect::Skip,
            &evidence,
        )
        .await;
    }
    reconcile(pool, LearnedFactKind::SelfUpdaterSuspected, &su_subjects).await;

    // Failing methods → DeprioritiseMethod.
    let failing = detect::detect_method_failing(&rows, QUORUM);
    let mf_subjects: std::collections::HashSet<String> =
        failing.iter().map(|c| c.subject()).collect();
    for c in &failing {
        let evidence = format!(
            "{} failures via {} (another method succeeded) in {WINDOW_DAYS}d",
            c.failures, c.method
        );
        let effect = Effect::DeprioritiseMethod {
            method: c.method.clone(),
        };
        upsert(
            pool,
            LearnedFactKind::MethodFailing,
            &c.subject(),
            &effect,
            &evidence,
        )
        .await;
    }
    reconcile(pool, LearnedFactKind::MethodFailing, &mf_subjects).await;
}

/// Decision-loop detection: learn ineffective fixes and user-rejected actions from the
/// execution-feedback and approval-rejection history. Feeds the issue-analysis confidence
/// gate and the learned-facts prompt section. Best-effort.
pub async fn analyse_issues(pool: &SqlitePool) {
    // Fixes that run but never improve the system → confidence penalty by action type.
    match store::fix_feedback_rows(pool, WINDOW_DAYS).await {
        Ok(rows) => {
            let ineffective = detect::detect_fix_ineffective(&rows, QUORUM);
            let subjects: std::collections::HashSet<String> =
                ineffective.iter().map(|c| c.subject.clone()).collect();
            for c in &ineffective {
                let evidence = format!(
                    "ran {} times with no improvement in {WINDOW_DAYS}d",
                    c.occurrences
                );
                let effect = Effect::ConfidencePenalty {
                    amount: MAX_CONFIDENCE_PENALTY,
                };
                upsert(
                    pool,
                    LearnedFactKind::FixIneffective,
                    &c.subject,
                    &effect,
                    &evidence,
                )
                .await;
            }
            reconcile(pool, LearnedFactKind::FixIneffective, &subjects).await;
        }
        Err(e) => warn!("self-improvement: reading fix feedback failed: {e}"),
    }

    // Actions the user keeps rejecting → confidence penalty by action label.
    match store::rejection_rows(pool, WINDOW_DAYS).await {
        Ok(rows) => {
            let rejected = detect::detect_rejected(&rows, QUORUM);
            let subjects: std::collections::HashSet<String> =
                rejected.iter().map(|c| c.subject.clone()).collect();
            for c in &rejected {
                let evidence = format!("rejected {} times in {WINDOW_DAYS}d", c.rejections);
                let effect = Effect::ConfidencePenalty {
                    amount: MAX_CONFIDENCE_PENALTY,
                };
                upsert(
                    pool,
                    LearnedFactKind::RejectedSignal,
                    &c.subject,
                    &effect,
                    &evidence,
                )
                .await;
            }
            reconcile(pool, LearnedFactKind::RejectedSignal, &subjects).await;
        }
        Err(e) => warn!("self-improvement: reading rejections failed: {e}"),
    }
}

async fn upsert(
    pool: &SqlitePool,
    kind: LearnedFactKind,
    subject: &str,
    effect: &Effect,
    evidence: &str,
) {
    if let Err(e) = store::upsert_fact(
        pool,
        kind.as_token(),
        subject,
        &effect.to_json(),
        evidence,
        WINDOW_DAYS,
    )
    .await
    {
        warn!(
            "self-improvement: persisting {} {subject} failed: {e}",
            kind.as_token()
        );
    } else {
        info!(
            kind = kind.as_token(),
            subject, "self-improvement: learned a machine pattern"
        );
    }
}

async fn reconcile(
    pool: &SqlitePool,
    kind: LearnedFactKind,
    current: &std::collections::HashSet<String>,
) {
    if let Err(e) = store::expire_unsupported(pool, kind.as_token(), current).await {
        warn!(
            "self-improvement: expiring stale {} facts failed: {e}",
            kind.as_token()
        );
    }
}

/// The `\u{1f}` (unit separator) joins an app id and a method into a single subject key
/// for `MethodFailing`, and is split back out by [`apply`]. It cannot occur in either part.
pub(crate) const SUBJECT_SEP: char = '\u{1f}';
