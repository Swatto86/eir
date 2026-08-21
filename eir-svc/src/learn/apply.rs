//! Applying learned facts at the decision seams. [`LearnedFacts`] is loaded once per
//! cycle from the in-force facts and consulted in memory (no per-decision DB hit). All
//! effects are conservative: deprioritise a method, or shave a capped amount off a
//! proposed action's confidence before the existing policy gate.

use super::detect::action_type;
use super::{store, Effect, LearnedFactKind, MAX_CONFIDENCE_PENALTY, SUBJECT_SEP};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

/// Security action types are never confidence-penalised by learning — a learned fact must
/// not be able to suppress a protective fix. (CamelCase variant names, matching the
/// Debug form used as the learned-fact key; keep in sync with `models::FixAction`.)
const SECURITY_ACTION_TYPES: &[&str] = &[
    "FirewallEnable",
    "DefenderSignatureUpdate",
    "DefenderRealtimeEnable",
];

/// Whether `subject` (an action label or action type) is a protective security action,
/// which learning must never suppress — via the confidence gate OR the prompt summary.
fn is_security(subject: &str) -> bool {
    SECURITY_ACTION_TYPES.contains(&action_type(subject))
}

/// An in-memory snapshot of the learned facts in force, for the decision seams.
#[derive(Default)]
pub struct LearnedFacts {
    /// (app base id, method token) pairs to push to the back of the heal order.
    method_deprioritised: HashSet<(String, String)>,
    /// FixIneffective: action TYPE → confidence penalty.
    penalty_by_type: HashMap<String, f32>,
    /// RejectedSignal: exact action label OR stable dedup key → confidence penalty.
    penalty_by_subject: HashMap<String, f32>,
    /// Plain-English lines describing what Eir has learned, for the issue-analysis prompt.
    summaries: Vec<String>,
}

impl LearnedFacts {
    /// Load the in-force facts (`active`/`user_pinned`). On a DB error, returns an empty
    /// set — learning never blocks the decision loop.
    pub async fn load(pool: &SqlitePool) -> Self {
        let mut f = LearnedFacts::default();
        let rows = match store::active_facts(pool).await {
            Ok(r) => r,
            Err(_) => return f,
        };
        for row in rows {
            let effect: Option<Effect> = serde_json::from_str(&row.effect_json).ok();
            match (LearnedFactKind::from_token(&row.kind), effect) {
                (Some(LearnedFactKind::SelfUpdaterSuspected), _) => {
                    f.summaries.push(format!(
                        "Not managing updates for '{}' — it self-updates ({}).",
                        row.subject, row.evidence
                    ));
                }
                (
                    Some(LearnedFactKind::MethodFailing),
                    Some(Effect::DeprioritiseMethod { method }),
                ) => {
                    if let Some((app, _)) = row.subject.split_once(SUBJECT_SEP) {
                        f.method_deprioritised
                            .insert((app.to_string(), method.clone()));
                        f.summaries.push(format!(
                            "Updating '{app}' avoids '{method}' ({}).",
                            row.evidence
                        ));
                    }
                }
                (
                    Some(LearnedFactKind::FixIneffective),
                    Some(Effect::ConfidencePenalty { amount }),
                ) if !is_security(&row.subject) => {
                    f.penalty_by_type.insert(row.subject.clone(), amount);
                    f.summaries.push(format!(
                        "Lower confidence in '{}' fixes here — {}.",
                        row.subject, row.evidence
                    ));
                }
                (
                    Some(LearnedFactKind::RejectedSignal),
                    Some(Effect::ConfidencePenalty { amount }),
                ) if !is_security(&row.subject) => {
                    f.penalty_by_subject.insert(row.subject.clone(), amount);
                    f.summaries.push(format!(
                        "You've repeatedly rejected '{}' ({}) — proposing it less readily.",
                        row.subject, row.evidence
                    ));
                }
                _ => {} // unknown kind/effect (e.g. a newer-build row) — ignored, not trusted
            }
        }
        f
    }

    /// Whether `method` should be tried last for `app_base` (a learned MethodFailing).
    pub fn is_method_deprioritised(&self, app_base: &str, method: &str) -> bool {
        self.method_deprioritised
            .contains(&(app_base.to_string(), method.to_string()))
    }

    /// The capped confidence penalty for a proposed action, summing the by-type
    /// (FixIneffective) and by-subject (RejectedSignal) penalties. `action_key` is the
    /// stable [`crate::models::FixAction::dedup_key`]; either that or the Debug label may
    /// match a stored RejectedSignal subject. Always 0 for security action types —
    /// learning never weakens a protective fix.
    pub fn confidence_penalty(&self, action_label: &str, action_key: &str) -> f32 {
        if is_security(action_label) {
            return 0.0;
        }
        let at = action_type(action_label);
        let by_type = self.penalty_by_type.get(at).copied().unwrap_or(0.0);
        let by_subject = self
            .penalty_by_subject
            .get(action_label)
            .or_else(|| {
                if action_key.is_empty() {
                    None
                } else {
                    self.penalty_by_subject.get(action_key)
                }
            })
            .copied()
            .unwrap_or(0.0);
        let bounded = |amount: f32| {
            if amount.is_finite() {
                amount.clamp(0.0, MAX_CONFIDENCE_PENALTY)
            } else {
                0.0
            }
        };
        (bounded(by_type) + bounded(by_subject)).min(MAX_CONFIDENCE_PENALTY)
    }

    /// A read-only "what Eir has learned on this machine" block for the issue-analysis
    /// prompt, so the diagnostician reasons with the same knowledge. None when nothing
    /// has been learned yet.
    pub fn prompt_section(&self) -> Option<String> {
        if self.summaries.is_empty() {
            return None;
        }
        let mut s = String::from(
            "\nWHAT EIR HAS LEARNED ON THIS MACHINE (from its own history — take into account):\n",
        );
        for line in &self.summaries {
            s.push_str("  - ");
            s.push_str(line);
            s.push('\n');
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_with(penalty_type: &[(&str, f32)], penalty_label: &[(&str, f32)]) -> LearnedFacts {
        LearnedFacts {
            method_deprioritised: HashSet::new(),
            penalty_by_type: penalty_type
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            penalty_by_subject: penalty_label
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            summaries: vec![],
        }
    }

    #[test]
    fn penalty_sums_type_and_label_and_caps() {
        let f = facts_with(
            &[("ServiceRestart", 0.1)],
            &[("ServiceRestart { service_name: \"x\" }", 0.1)],
        );
        // 0.1 + 0.1 = 0.2, capped to MAX_CONFIDENCE_PENALTY.
        let p = f.confidence_penalty("ServiceRestart { service_name: \"x\" }", "");
        assert!((p - MAX_CONFIDENCE_PENALTY).abs() < 1e-6);
        // A different instance of the same type gets only the by-type penalty.
        assert!(
            (f.confidence_penalty("ServiceRestart { service_name: \"y\" }", "") - 0.1).abs() < 1e-6
        );
    }

    #[test]
    fn rejected_signal_matches_stable_action_key() {
        let f = facts_with(&[], &[("process_kill|chrome", 0.15)]);
        assert!(
            (f.confidence_penalty(
                "ProcessKill { process_name: \"chrome\" }",
                "process_kill|chrome"
            ) - 0.15)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn security_actions_are_never_penalised() {
        let f = facts_with(
            &[("DefenderRealtimeEnable", 0.15)],
            &[("DefenderRealtimeEnable", 0.15)],
        );
        assert_eq!(f.confidence_penalty("DefenderRealtimeEnable", ""), 0.0);
    }

    #[test]
    fn unknown_action_has_no_penalty() {
        let f = facts_with(&[], &[]);
        assert_eq!(
            f.confidence_penalty("DiskCleanup { target: \"temp\" }", "disk_cleanup|temp"),
            0.0
        );
    }

    #[test]
    fn malformed_penalties_can_never_raise_or_poison_confidence() {
        let negative = facts_with(&[("ServiceRestart", -0.2)], &[]);
        assert_eq!(negative.confidence_penalty("ServiceRestart", ""), 0.0);

        let non_finite = facts_with(&[("ServiceRestart", f32::NAN)], &[]);
        assert_eq!(non_finite.confidence_penalty("ServiceRestart", ""), 0.0);
    }

    #[test]
    fn is_security_recognises_action_types_from_labels() {
        assert!(is_security("FirewallEnable { profile: \"all\" }"));
        assert!(is_security("DefenderRealtimeEnable"));
        assert!(!is_security("ProcessKill { process_name: \"x\" }"));
    }

    #[tokio::test]
    async fn load_excludes_rejected_security_actions_from_prompt_and_penalty() {
        let path = std::env::temp_dir().join(format!("eir-apply-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!(
            "sqlite:{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.expect("open db");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let pen = serde_json::to_string(&Effect::ConfidencePenalty { amount: 0.15 }).unwrap();
        let insert = |label: &str| {
            let pen = pen.clone();
            let label = label.to_string();
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO learned_facts (kind, subject, effect_json, evidence_json, \
                     first_seen_at, last_reinforced_at, status, source) \
                     VALUES ('rejected_signal', ?, ?, '3 rejections', '2026-01-01T00:00:00Z', \
                     '2026-01-01T00:00:00Z', 'active', 'detector')",
                )
                .bind(label)
                .bind(pen)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        insert("FirewallEnable { profile: \"all\" }").await;
        insert("ProcessKill { process_name: \"x\" }").await;

        let facts = LearnedFacts::load(&pool).await;
        // The security rejection is neither penalised nor surfaced to the diagnostician.
        assert_eq!(
            facts.confidence_penalty("FirewallEnable { profile: \"all\" }", "firewall_enable|all"),
            0.0
        );
        let prompt = facts.prompt_section().unwrap_or_default();
        assert!(
            !prompt.contains("FirewallEnable"),
            "security action leaked into prompt: {prompt}"
        );
        // The ordinary rejection is still learned and surfaced.
        assert!(
            facts.confidence_penalty("ProcessKill { process_name: \"x\" }", "process_kill|x") > 0.0
        );
        assert!(prompt.contains("ProcessKill"));
        drop(pool);
    }
}
