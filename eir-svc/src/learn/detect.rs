//! Pure pattern detectors over already-recorded audit rows. No I/O, no AI — fully
//! unit-testable with row case tables.

use crate::learn::SUBJECT_SEP;
use crate::updater::check::base_id;
use crate::updater::proc::TIMED_OUT;
use std::collections::{HashMap, HashSet};

// ── Updater-cycle evidence (update_attempts) ────────────────────────────────────

/// One update attempt, as read from `update_attempts` (the columns the detectors need).
#[derive(Debug, Clone)]
pub struct AttemptRow {
    pub app_id: String,
    pub cycle_id: i64,
    pub success: bool,
    pub exit_code: Option<i32>,
    /// Failure category as the stored snake_case token (NULL/None on success).
    pub category: Option<String>,
    pub method: String,
    pub detail: String,
}

/// A learned self-updater: an app whose package-manager update keeps timing out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdaterCandidate {
    /// `base_id` of the app (choco `.install`/etc. variants collapsed to one identity).
    pub subject: String,
    /// Distinct cycles in which the update timed out — the strength of the evidence.
    pub timeout_cycles: i64,
}

/// A method that keeps failing for an app while another method works for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodFailingCandidate {
    pub app: String,
    pub method: String,
    pub failures: i64,
}

impl MethodFailingCandidate {
    /// The composite subject key `"<app><US><method>"` stored in `learned_facts`.
    pub fn subject(&self) -> String {
        format!("{}{}{}", self.app, SUBJECT_SEP, self.method)
    }
}

/// True when an attempt represents a package-manager TIMEOUT — the self-updater signal.
/// Requiring the `network_transient` category is essential: the exit code `-4` is ALSO
/// used by the native method to abort when a staged installer's hash changed (a tamper
/// guard, category `hash_mismatch`) — keying on the bare code would mislabel a tampering
/// signal as "this app self-updates".
fn is_timeout(r: &AttemptRow) -> bool {
    !r.success
        && r.category.as_deref() == Some("network_transient")
        && (r.exit_code == Some(TIMED_OUT) || r.detail.to_lowercase().contains("timed out"))
}

/// Apps that timed out in at least `quorum` distinct cycles AND never succeeded in the
/// window. The zero-successes guard stops a single bad-release week from sidelining a
/// healthy app.
pub fn detect_self_updaters(rows: &[AttemptRow], quorum: i64) -> Vec<SelfUpdaterCandidate> {
    struct Agg {
        timeout_cycles: HashSet<i64>,
        successes: i64,
    }
    let mut by_app: HashMap<String, Agg> = HashMap::new();
    for r in rows {
        let agg = by_app.entry(base_id(&r.app_id).to_string()).or_insert(Agg {
            timeout_cycles: HashSet::new(),
            successes: 0,
        });
        if r.success {
            agg.successes += 1;
        } else if is_timeout(r) {
            agg.timeout_cycles.insert(r.cycle_id);
        }
    }
    let mut out: Vec<SelfUpdaterCandidate> = by_app
        .into_iter()
        .filter(|(_, a)| a.successes == 0 && (a.timeout_cycles.len() as i64) >= quorum)
        .map(|(subject, a)| SelfUpdaterCandidate {
            subject,
            timeout_cycles: a.timeout_cycles.len() as i64,
        })
        .collect();
    out.sort_by_key(|c| c.subject.clone()); // deterministic order (small vec)
    out
}

/// Failure categories that genuinely indict the METHOD itself (vs the app, the network,
/// or an integrity/tamper signal). ONLY these count toward deprioritising a method — a
/// `hash_mismatch`/`signature_rejected` must never teach Eir to avoid the method that
/// caught a tampered or unsigned installer, and transient/app-level categories
/// (network, reboot, lock, already-current, blocked) aren't the method's fault either.
const METHOD_FAULT_CATEGORIES: &[&str] = &[
    "installer_failed",
    "verify_failed",
    "not_found",
    "needs_force",
    "permission_denied",
];

fn is_method_fault(r: &AttemptRow) -> bool {
    !r.success
        && r.category
            .as_deref()
            .map(|c| METHOD_FAULT_CATEGORIES.contains(&c))
            .unwrap_or(false)
}

/// Per (app, method): the method failed (with a method-attributable category) >= `quorum`
/// times with zero successes via that method, AND some OTHER method succeeded for the same
/// app in the window. The cross-method success guard distinguishes "this method is bad for
/// this app" from "this app is simply unupdatable" (the self-updater detector's job); the
/// category filter keeps an integrity-gate rejection from ever deprioritising the method
/// that performs the gate.
pub fn detect_method_failing(rows: &[AttemptRow], quorum: i64) -> Vec<MethodFailingCandidate> {
    #[derive(Default)]
    struct Agg {
        failures: i64,
        successes: i64,
    }
    let mut by_app_method: HashMap<(String, String), Agg> = HashMap::new();
    let mut app_success: HashMap<String, bool> = HashMap::new();
    for r in rows {
        let app = base_id(&r.app_id).to_string();
        if r.success {
            by_app_method
                .entry((app.clone(), r.method.clone()))
                .or_default()
                .successes += 1;
            app_success.insert(app, true);
        } else if is_method_fault(r) {
            by_app_method
                .entry((app.clone(), r.method.clone()))
                .or_default()
                .failures += 1;
            app_success.entry(app).or_insert(false);
        }
        // Non-method-fault failures (hash_mismatch, signature_rejected, network_transient,
        // needs_reboot, lock_held, already_current, blocked, unknown) are ignored here.
    }
    let mut out: Vec<MethodFailingCandidate> = by_app_method
        .into_iter()
        .filter(|((app, _), a)| {
            a.successes == 0 && a.failures >= quorum && *app_success.get(app).unwrap_or(&false)
            // another method worked
        })
        .map(|((app, method), a)| MethodFailingCandidate {
            app,
            method,
            failures: a.failures,
        })
        .collect();
    out.sort_by_key(|c| c.subject()); // deterministic order (small vec)
    out
}

// ── Decision-loop evidence (execution_feedback, approval_rejections) ─────────────

/// One execution-feedback row. Effectiveness is judged by the fix's *targeted outcome*
/// (`resolved`: did the specific fault it addressed clear?), not the blended
/// improvement_score and not the global failed-services count — cpu/mem deltas are
/// machine-wide noise, and the global count can stay flat when this fix's target cleared
/// but an unrelated service newly failed, wrongly branding an effective fix ineffective.
#[derive(Debug, Clone)]
pub struct FeedbackRow {
    /// The executed action's Debug form (e.g. `ServiceRestart { service_name: "X" }`).
    pub action: String,
    pub succeeded: bool,
    /// Whether the fix's targeted fault cleared by the settle-windowed after-measurement.
    /// `Some(true)` = cleared, `Some(false)` = didn't, `None` = not metric-checkable / not
    /// yet measured (ignored by the detector — never counted as ineffective).
    pub resolved: Option<bool>,
}

/// Action types whose effectiveness the failed-services count can actually measure.
/// Restricting FixIneffective to these avoids penalising fixes whose effect this metric
/// can't see (and never touches security actions).
const SERVICE_FIX_TYPES: &[&str] = &["ServiceRestart", "ServiceStart"];

/// One approval-rejection row: an action the user rejected.
#[derive(Debug, Clone)]
pub struct RejectionRow {
    /// The action's Debug form (`format!("{action:?}")`), the same key the loop uses.
    pub action_label: String,
}

/// A fix action TYPE that keeps "succeeding" without improving the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixIneffectiveCandidate {
    pub subject: String, // action type (the variant name)
    pub occurrences: i64,
}

/// An action the user keeps rejecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedCandidate {
    pub subject: String, // the action label
    pub rejections: i64,
}

/// The action TYPE (the leading variant name) of a Debug-form action string, so
/// `ServiceRestart { service_name: "X" }` and `ServiceRestart { service_name: "Y" }`
/// aggregate together. Falls back to the whole trimmed string for a unit variant.
pub fn action_type(action_label: &str) -> &str {
    let end = action_label
        .find([' ', '{', '('])
        .unwrap_or(action_label.len());
    action_label[..end].trim()
}

/// A service-fix action TYPE that "succeeded" >= `quorum` times yet NEVER reduced the
/// failed-service count (zero effective instances in the window) — the type goes through
/// the motions without ever clearing a fault on this machine. The zero-effective guard
/// (mirroring the zero-success guards elsewhere) confines the penalty to types that never
/// help, so persistently-broken services can't suppress a legitimate restart of a healthy
/// one. Restricted to SERVICE_FIX_TYPES and to measured rows.
pub fn detect_fix_ineffective(rows: &[FeedbackRow], quorum: i64) -> Vec<FixIneffectiveCandidate> {
    #[derive(Default)]
    struct Agg {
        ineffective: i64,
        effective: i64,
    }
    let mut by_type: HashMap<String, Agg> = HashMap::new();
    for r in rows {
        if !r.succeeded {
            continue;
        }
        let at = action_type(&r.action);
        if !SERVICE_FIX_TYPES.contains(&at) {
            continue;
        }
        // Judge by the targeted outcome: did the specific fault this fix addressed clear?
        // `None` (not metric-checkable / unmeasured) is ignored — never counted ineffective.
        if let Some(resolved) = r.resolved {
            let agg = by_type.entry(at.to_string()).or_default();
            if resolved {
                agg.effective += 1;
            } else {
                agg.ineffective += 1;
            }
        }
    }
    let mut out: Vec<FixIneffectiveCandidate> = by_type
        .into_iter()
        .filter(|(_, a)| a.effective == 0 && a.ineffective >= quorum)
        .map(|(subject, a)| FixIneffectiveCandidate {
            subject,
            occurrences: a.ineffective,
        })
        .collect();
    out.sort_by_key(|c| c.subject.clone()); // deterministic order (small vec)
    out
}

/// Action labels / keys the user rejected >= `quorum` times. Prefer stable
/// `FixAction::dedup_key` subjects (migration 0020+) so regenerable Debug parameters
/// cannot fragment the count below quorum.
pub fn detect_rejected(rows: &[RejectionRow], quorum: i64) -> Vec<RejectedCandidate> {
    let mut by_label: HashMap<String, i64> = HashMap::new();
    for r in rows {
        *by_label.entry(r.action_label.clone()).or_insert(0) += 1;
    }
    let mut out: Vec<RejectedCandidate> = by_label
        .into_iter()
        .filter(|(_, n)| *n >= quorum)
        .map(|(subject, rejections)| RejectedCandidate {
            subject,
            rejections,
        })
        .collect();
    out.sort_by_key(|c| c.subject.clone()); // deterministic order (small vec)
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arow(
        app: &str,
        cycle: i64,
        success: bool,
        exit: Option<i32>,
        cat: &str,
        method: &str,
        detail: &str,
    ) -> AttemptRow {
        AttemptRow {
            app_id: app.to_string(),
            cycle_id: cycle,
            success,
            exit_code: exit,
            category: if cat.is_empty() {
                None
            } else {
                Some(cat.to_string())
            },
            method: method.to_string(),
            detail: detail.to_string(),
        }
    }
    fn timeout_row(app: &str, cycle: i64) -> AttemptRow {
        arow(
            app,
            cycle,
            false,
            Some(TIMED_OUT),
            "network_transient",
            "choco",
            "command timed out after 600s",
        )
    }

    #[test]
    fn three_timeout_cycles_zero_successes_is_a_self_updater() {
        let rows = vec![
            timeout_row("discord.install", 1),
            timeout_row("discord.install", 2),
            timeout_row("discord", 3),
        ];
        let got = detect_self_updaters(&rows, 3);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "discord");
    }

    #[test]
    fn category_fallback_detects_timeout_without_the_exit_code() {
        let rows = vec![
            arow(
                "foo",
                1,
                false,
                None,
                "network_transient",
                "winget",
                "the request timed out",
            ),
            arow(
                "foo",
                2,
                false,
                None,
                "network_transient",
                "winget",
                "command timed out",
            ),
            arow(
                "foo",
                3,
                false,
                None,
                "network_transient",
                "winget",
                "timed out and terminated",
            ),
        ];
        assert_eq!(detect_self_updaters(&rows, 3).len(), 1);
    }

    #[test]
    fn any_success_in_window_disqualifies_self_updater() {
        let rows = vec![
            timeout_row("vlc", 1),
            timeout_row("vlc", 2),
            timeout_row("vlc", 3),
            arow("vlc", 4, true, Some(0), "", "choco", "updated"),
        ];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }

    #[test]
    fn native_tamper_abort_is_not_a_timeout() {
        // exit -4 (== TIMED_OUT) but category hash_mismatch must NOT count as a timeout.
        let rows = vec![
            arow(
                "app",
                1,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "native",
                "staged installer changed before launch — aborted (possible tampering)",
            ),
            arow(
                "app",
                2,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "native",
                "staged installer changed before launch — aborted (possible tampering)",
            ),
            arow(
                "app",
                3,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "native",
                "staged installer changed before launch — aborted (possible tampering)",
            ),
        ];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }

    #[test]
    fn method_failing_needs_a_cross_method_success() {
        // choco fails 3x, native succeeds once → choco is the failing method for "app".
        let rows = vec![
            arow(
                "app",
                1,
                false,
                Some(1),
                "installer_failed",
                "choco",
                "exit 1",
            ),
            arow(
                "app",
                2,
                false,
                Some(1),
                "installer_failed",
                "choco",
                "exit 1",
            ),
            arow(
                "app",
                3,
                false,
                Some(1),
                "installer_failed",
                "choco",
                "exit 1",
            ),
            arow("app", 3, true, Some(0), "", "native", "installed"),
        ];
        let got = detect_method_failing(&rows, 3);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "choco");
        assert!(got[0].subject().contains("choco"));
    }

    #[test]
    fn all_methods_failing_is_not_a_method_fact() {
        // No method succeeded → not a single-method problem (it's a self-updater / unupdatable).
        let rows = vec![
            arow(
                "app",
                1,
                false,
                Some(1),
                "installer_failed",
                "choco",
                "exit 1",
            ),
            arow(
                "app",
                2,
                false,
                Some(1),
                "installer_failed",
                "choco",
                "exit 1",
            ),
            arow(
                "app",
                3,
                false,
                Some(1),
                "installer_failed",
                "choco",
                "exit 1",
            ),
        ];
        assert!(detect_method_failing(&rows, 3).is_empty());
    }

    #[test]
    fn method_failing_ignores_integrity_failures() {
        // The native method rejecting a tampered/unsigned installer (hash_mismatch /
        // signature_rejected) must NEVER deprioritise native, even though choco succeeds.
        let rows = vec![
            arow(
                "app",
                1,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "native",
                "tampered",
            ),
            arow(
                "app",
                2,
                false,
                Some(1),
                "signature_rejected",
                "native",
                "unsigned",
            ),
            arow(
                "app",
                3,
                false,
                Some(1),
                "hash_mismatch",
                "native",
                "tampered",
            ),
            arow("app", 3, true, Some(0), "", "choco", "installed"),
        ];
        assert!(detect_method_failing(&rows, 3).is_empty());
    }

    fn fb(action: &str, ok: bool, resolved: Option<bool>) -> FeedbackRow {
        FeedbackRow {
            action: action.to_string(),
            succeeded: ok,
            resolved,
        }
    }

    #[test]
    fn fix_ineffective_when_a_service_type_never_helps() {
        // 3 succeeded ServiceRestarts whose targeted service never cleared → penalty.
        let rows = vec![
            fb("ServiceRestart { service_name: \"A\" }", true, Some(false)),
            fb("ServiceRestart { service_name: \"B\" }", true, Some(false)),
            fb("ServiceRestart { service_name: \"C\" }", true, Some(false)),
        ];
        let got = detect_fix_ineffective(&rows, 3);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "ServiceRestart");
        assert_eq!(got[0].occurrences, 3);
    }

    #[test]
    fn fix_ineffective_spared_when_the_type_ever_helps() {
        // 3 that didn't clear their target but ALSO one that DID → the type sometimes works,
        // so it must NOT be penalised (avoids suppressing a healthy service's restart).
        let rows = vec![
            fb("ServiceRestart { service_name: \"A\" }", true, Some(false)),
            fb("ServiceRestart { service_name: \"B\" }", true, Some(false)),
            fb("ServiceRestart { service_name: \"C\" }", true, Some(false)),
            fb("ServiceRestart { service_name: \"D\" }", true, Some(true)), // cleared its target
        ];
        assert!(detect_fix_ineffective(&rows, 3).is_empty());
    }

    #[test]
    fn fix_ineffective_ignores_non_service_types_and_failed_and_unmeasured() {
        let rows = vec![
            // A non-service fix is out of scope (this metric can't judge it).
            fb("DiskCleanup { target: \"temp\" }", true, Some(false)),
            fb("DiskCleanup { target: \"temp\" }", true, Some(false)),
            fb("DiskCleanup { target: \"temp\" }", true, Some(false)),
            // Security actions are never penalised by this metric.
            fb("DefenderSignatureUpdate", true, Some(false)),
            fb("DefenderSignatureUpdate", true, Some(false)),
            fb("DefenderSignatureUpdate", true, Some(false)),
            // Failed / unmeasured (resolved=None) service fixes don't count.
            fb("ServiceRestart { service_name: \"X\" }", false, Some(false)),
            fb("ServiceRestart { service_name: \"X\" }", true, None),
        ];
        assert!(detect_fix_ineffective(&rows, 3).is_empty());
    }

    #[test]
    fn rejected_aggregates_by_exact_label() {
        let rows = vec![
            RejectionRow {
                action_label: "ProcessKill { process_name: \"x\" }".into(),
            },
            RejectionRow {
                action_label: "ProcessKill { process_name: \"x\" }".into(),
            },
            RejectionRow {
                action_label: "ProcessKill { process_name: \"x\" }".into(),
            },
        ];
        let got = detect_rejected(&rows, 3);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rejections, 3);
    }

    #[test]
    fn action_type_extracts_variant_name() {
        assert_eq!(
            action_type("ServiceRestart { service_name: \"X\" }"),
            "ServiceRestart"
        );
        assert_eq!(
            action_type("DefenderSignatureUpdate"),
            "DefenderSignatureUpdate"
        );
        assert_eq!(
            action_type("NetworkDiagnostic { command: \"flush_dns\" }"),
            "NetworkDiagnostic"
        );
    }
}
