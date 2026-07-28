//! The AI diagnostician — the heart of "if it fails, read the error and try another
//! way". After a method fails, the model is shown the *real* captured error plus
//! what's been tried and what's still available, and proposes ONE next step. The
//! proposal is untrusted: Rust classified the error (not the AI), and
//! [`validate_next_step`] disposes — the AI may only pick an available, untried
//! method or an allow-listed remedy whose target it justified from the error text.
//! When the AI is unavailable or proposes nonsense, the deterministic ladder runs.

use crate::ai::client::{extract_json, AiClient};
use crate::updater::domain::{
    deterministic_next, validate_next_step, AttemptOutcome, ErrorCategory, Method, NextStep,
    ProposedStep, Remedy, StepContext, UpdateCandidate,
};
use serde::Deserialize;

/// The AI's raw, untrusted proposal.
#[derive(Deserialize, Default)]
struct RawStep {
    #[serde(default)]
    action: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    remedy: Option<RawRemedy>,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct RawRemedy {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
}

fn to_remedy(r: &RawRemedy) -> Option<Remedy> {
    match r.kind.trim().to_ascii_lowercase().as_str() {
        "force" => Some(Remedy::Force),
        "kill_process" | "killprocess" => Some(Remedy::KillProcess {
            name: r.name.trim().to_string(),
        }),
        "clear_manager_lock" | "clearmanagerlock" => Some(Remedy::ClearManagerLock),
        "retry_after_reboot" | "retryafterreboot" => Some(Remedy::RetryAfterReboot),
        _ => None,
    }
}

/// Map the raw proposal to a typed [`ProposedStep`]. An unparseable method/remedy
/// collapses to GiveUp, which the validator then turns into the deterministic next
/// step — so a malformed AI reply never strands the app.
fn to_proposed(raw: RawStep) -> ProposedStep {
    match raw.action.trim().to_ascii_lowercase().as_str() {
        "give_up" | "giveup" | "stop" => ProposedStep::GiveUp { reason: raw.reason },
        "retry" => match (
            Method::from_token(&raw.method),
            raw.remedy.as_ref().and_then(to_remedy),
        ) {
            (Some(method), Some(remedy)) => ProposedStep::Retry { method, remedy },
            _ => ProposedStep::GiveUp {
                reason: "AI retry proposal was incomplete".to_string(),
            },
        },
        // Default to "switch" — the common case — when the action is "switch" or
        // anything else, as long as a method parses.
        _ => match Method::from_token(&raw.method) {
            Some(method) => ProposedStep::Switch { method },
            None => ProposedStep::GiveUp {
                reason: "AI proposed an unknown method".to_string(),
            },
        },
    }
}

/// Parse a model response into a proposal. Pure — unit-tested against recorded
/// replies (fenced, prose-wrapped, malformed).
fn parse_step(content: &str) -> ProposedStep {
    match serde_json::from_str::<RawStep>(extract_json(content)) {
        Ok(raw) => to_proposed(raw),
        Err(_) => ProposedStep::GiveUp {
            reason: "could not parse the AI's next-step reply".to_string(),
        },
    }
}

fn method_list(methods: &[Method]) -> String {
    methods
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn diagnose_prompt(
    candidate: &UpdateCandidate,
    last: &AttemptOutcome,
    tried: &[Method],
    available: &[Method],
) -> String {
    let category = last
        .category
        .and_then(|c| serde_json::to_value(c).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let untried: Vec<Method> = available
        .iter()
        .copied()
        .filter(|m| !tried.contains(m))
        .collect();
    format!(
        "You are diagnosing a FAILED Windows app update so it can be retried a different way.\n\n\
APP: {name} (installed {current}, target {available_ver})\n\
Saved guidance: {guidance}\n\
The method \"{method}\" just failed.\n\
Failure category: {category}\n\
Error output: {detail}\n\
Methods already tried: {tried}\n\
Methods still available to try: {untried}\n\n\
Choose the single best next step. Respond ONLY with JSON, no markdown:\n\
{{\"action\":\"switch|retry|give_up\",\"method\":\"<one of the available methods>\",\
\"remedy\":{{\"kind\":\"force|kill_process|clear_manager_lock|retry_after_reboot\",\"name\":\"<exact process image name, ONLY for kill_process>\"}},\"reason\":\"<short>\"}}\n\n\
Rules:\n\
- \"switch\": try a DIFFERENT available method (best when this one can't handle the app).\n\
- \"retry\": re-run a method after a remedy — \"force\" if it refused a modified/portable package; \
\"kill_process\" (name MUST appear in the error output) if a file is locked; \"clear_manager_lock\" if \
a package-manager lock is held; \"retry_after_reboot\" if a reboot is required.\n\
- \"give_up\": if no available method can plausibly succeed.\n\
Follow saved guidance when it applies, but never bypass integrity checks. Only choose a method \
from the available list. Set remedy to null when switching.",
        name = candidate.name,
        current = candidate.current,
        available_ver = candidate.available,
        guidance = candidate.guidance.as_deref().unwrap_or("none"),
        method = last.method.as_str(),
        detail = last.detail,
        tried = method_list(tried),
        untried = method_list(&untried),
    )
}

/// Ask the AI for the next step after a failure and return the VALIDATED decision
/// plus the call's cost. Falls back to the deterministic ladder if the AI is
/// unreachable.
pub async fn diagnose(
    ai: &AiClient,
    model_override: &str,
    candidate: &UpdateCandidate,
    last: &AttemptOutcome,
    tried: &[Method],
    available: &[Method],
) -> (NextStep, f64) {
    let ctx = StepContext {
        failed: last.category.unwrap_or(ErrorCategory::Unknown),
        tried,
        available,
        error_text: &last.detail,
    };
    let prompt = diagnose_prompt(candidate, last, tried, available);
    let (content, usage) = match ai.complete(&prompt, model_override).await {
        Ok(v) => v,
        // AI down / errored -> deterministic next step, no cost.
        Err(_) => return (deterministic_next(&ctx), 0.0),
    };
    let cost = usage.map(|u| u.cost_usd).unwrap_or(0.0);
    (validate_next_step(parse_step(&content), &ctx), cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_switch_proposal() {
        let c =
            r#"{"action":"switch","method":"choco","remedy":null,"reason":"winget can't find it"}"#;
        assert_eq!(
            parse_step(c),
            ProposedStep::Switch {
                method: Method::Choco
            }
        );
    }

    #[test]
    fn parses_a_force_retry_from_fenced_json() {
        let c = "```json\n{\"action\":\"retry\",\"method\":\"winget\",\"remedy\":{\"kind\":\"force\"},\"reason\":\"modified portable\"}\n```";
        assert_eq!(
            parse_step(c),
            ProposedStep::Retry {
                method: Method::Winget,
                remedy: Remedy::Force
            }
        );
    }

    #[test]
    fn parses_a_kill_process_retry() {
        let c = r#"{"action":"retry","method":"winget","remedy":{"kind":"kill_process","name":"firefox.exe"},"reason":"locked"}"#;
        assert_eq!(
            parse_step(c),
            ProposedStep::Retry {
                method: Method::Winget,
                remedy: Remedy::KillProcess {
                    name: "firefox.exe".to_string()
                }
            }
        );
    }

    #[test]
    fn give_up_and_malformed_both_collapse_safely() {
        assert_eq!(
            parse_step(r#"{"action":"give_up","reason":"nothing works"}"#),
            ProposedStep::GiveUp {
                reason: "nothing works".to_string()
            }
        );
        // Not JSON at all -> a GiveUp the validator turns into the deterministic step.
        assert!(matches!(
            parse_step("I think you should try chocolatey"),
            ProposedStep::GiveUp { .. }
        ));
        // A retry missing its remedy is incomplete -> GiveUp.
        assert!(matches!(
            parse_step(r#"{"action":"retry","method":"winget","remedy":null}"#),
            ProposedStep::GiveUp { .. }
        ));
    }

    #[test]
    fn full_validation_rejects_unavailable_method_and_falls_back() {
        // The AI switches to a method that isn't available; the validator drops it to
        // the deterministic next (the first untried available method).
        let candidate = UpdateCandidate {
            id: "tool".into(),
            name: "Tool".into(),
            current: "1.0".into(),
            available: "2.0".into(),
            package_id: None,
            guidance: None,
            methods: vec![Method::Winget, Method::Native],
        };
        let last = AttemptOutcome::failed(Method::Winget, ErrorCategory::InstallerFailed, "boom");
        let ctx = StepContext {
            failed: ErrorCategory::InstallerFailed,
            tried: &[Method::Winget],
            available: &[Method::Winget, Method::Native],
            error_text: &last.detail,
        };
        // AI proposes Scoop (not available) -> deterministic fallback to Native.
        let proposed = parse_step(r#"{"action":"switch","method":"scoop"}"#);
        assert_eq!(
            validate_next_step(proposed, &ctx),
            NextStep::SwitchTo(Method::Native)
        );
        let _ = candidate; // keeps the realistic shape in view
    }

    #[test]
    fn retry_diagnosis_includes_saved_guidance() {
        let candidate = UpdateCandidate {
            id: "tool".into(),
            name: "Tool".into(),
            current: "1.0".into(),
            available: "2.0".into(),
            package_id: None,
            guidance: Some("Use the signed x64 asset".into()),
            methods: vec![Method::Winget, Method::Native],
        };
        let last = AttemptOutcome::failed(Method::Winget, ErrorCategory::InstallerFailed, "exit 1");
        let prompt = diagnose_prompt(
            &candidate,
            &last,
            &[Method::Winget],
            &[Method::Winget, Method::Native],
        );
        assert!(prompt.contains("Saved guidance: Use the signed x64 asset"));
    }
}
