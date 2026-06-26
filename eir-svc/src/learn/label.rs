//! Tier-2 (optional) AI labeller. Attaches a one-sentence, human-readable explanation to
//! a deterministically-derived learned fact, so the UI card reads naturally. The AI is
//! READ-ONLY here: it only produces explanation TEXT — it never creates a fact, changes
//! its kind/subject/effect, or influences any decision. The fact and its behaviour are
//! decided entirely by the deterministic detectors; this is pure presentation, mirroring
//! how advisor mode lets the model advise while Rust keeps authority.
//!
//! Bounded by construction: only facts that lack an explanation are labelled, and at most
//! one per call — so the total number of AI calls is at worst the number of distinct facts
//! the machine ever forms, and steady state is zero.

use super::store;
use crate::ai::client::AiClient;
use sqlx::SqlitePool;
use tracing::warn;

/// Cap on the stored explanation, so a verbose model can't bloat the card or the prompt.
const MAX_EXPLANATION: usize = 200;

/// Label at most one unlabelled fact. Returns true if a fact was labelled (a fresh AI
/// explanation was stored). Best-effort: any failure is logged and ignored.
pub async fn label_one(pool: &SqlitePool, ai: &AiClient, model: &str) -> bool {
    let (id, kind, subject, evidence) = match store::next_unlabelled(pool).await {
        Ok(Some(f)) => f,
        Ok(None) => return false, // every fact already attempted — no AI call
        Err(e) => {
            warn!("self-improvement: querying unlabelled facts failed: {e}");
            return false;
        }
    };

    // Mark the attempt up front so this fact is never re-queried, even if the call errors
    // or the reply is unusable — that is what keeps total AI calls to one per fact and
    // stops one un-labellable fact looping (and starving the others) every cycle.
    if let Err(e) = store::mark_label_attempted(pool, id).await {
        warn!("self-improvement: marking label attempt failed: {e}");
        return false;
    }

    let prompt = format!(
        "You explain, in ONE short plain-English sentence for a non-expert, why a Windows \
         maintenance agent has learned the following about THIS machine and how it will now \
         behave. Be factual and concise; do not invent specifics beyond what is given. \
         Respond with ONLY the sentence — no preamble, no quotes, no markdown.\n\n\
         Learned-fact kind: {kind}\nSubject: {subject}\nEvidence: {evidence}"
    );

    let content = match ai.complete(&prompt, model).await {
        Ok((c, _usage)) => c,
        Err(e) => {
            warn!("self-improvement: AI labelling failed: {e}");
            return false;
        }
    };

    let explanation = sanitise(&content);
    if explanation.is_empty() {
        return false;
    }
    if let Err(e) = store::set_ai_explanation(pool, id, &explanation).await {
        warn!("self-improvement: storing AI explanation failed: {e}");
        return false;
    }
    true
}

/// Collapse the model's reply to a single trimmed, length-capped line.
fn sanitise(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line.trim().trim_matches('"').trim();
    if trimmed.chars().count() > MAX_EXPLANATION {
        format!(
            "{}…",
            trimmed.chars().take(MAX_EXPLANATION).collect::<String>()
        )
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_collapses_and_caps() {
        assert_eq!(sanitise("  hello\n  world  "), "hello world");
        assert_eq!(sanitise("\"quoted reply\""), "quoted reply");
        assert!(sanitise("").is_empty());
        let long = "x ".repeat(300);
        let out = sanitise(&long);
        assert!(out.chars().count() <= MAX_EXPLANATION + 1); // +1 for the ellipsis
    }
}
