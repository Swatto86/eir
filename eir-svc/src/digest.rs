//! The weekly plain-English health digest: one bounded AI call over aggregated audit
//! counts (never raw snapshots), turning the otherwise-silent guardian's week into a
//! short retrospective the user can read.

use crate::ai::client::AiClient;
use crate::audit::{self, DigestStats};
use crate::models::CallUsage;
use anyhow::Result;
use chrono::{Duration, Utc};
use sqlx::SqlitePool;

/// Aggregate the last 7 days and ask the model for a short digest. Returns the digest
/// text plus the call's usage (so the caller can bill it), leaving persistence to the
/// caller.
pub async fn generate(
    db: &SqlitePool,
    ai: &AiClient,
    model: &str,
) -> Result<(String, Option<CallUsage>)> {
    let cutoff = (Utc::now() - Duration::days(7)).to_rfc3339();
    let stats = audit::digest_stats(db, &cutoff).await?;
    let prompt = build_prompt(&stats);
    let (text, usage) = ai.complete_text(&prompt, model).await?;
    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("digest model returned empty text");
    }
    Ok((text, usage))
}

/// The bounded digest prompt (pure, testable). Only aggregate counts go in — no raw
/// diagnostics or snapshots.
fn build_prompt(s: &DigestStats) -> String {
    format!(
        "You are Eir, a Windows guardian agent. Write a SHORT weekly health digest (3-4 \
         sentences, plain English, for a non-technical home PC owner) summarising the past 7 \
         days from these counts. Be reassuring when quiet, specific when not. Do NOT invent \
         details beyond the numbers. No preamble, no markdown, no bullet list — just the \
         paragraph.\n\n\
         - Analyses run: {decisions}\n\
         - Fixes applied: {exec_total} ({exec_success} succeeded)\n\
         - App updates attempted: {updates_total} ({updates_success} succeeded)\n\
         - Patterns learned about this machine: {learned_facts}\n\
         - Estimated AI spend: ${spend:.2}\n",
        decisions = s.decisions,
        exec_total = s.exec_total,
        exec_success = s.exec_success,
        updates_total = s.updates_total,
        updates_success = s.updates_success,
        learned_facts = s.learned_facts,
        spend = s.spend_usd,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_every_count_and_no_raw_data() {
        let s = DigestStats {
            decisions: 42,
            exec_total: 5,
            exec_success: 4,
            updates_total: 3,
            updates_success: 3,
            learned_facts: 7,
            spend_usd: 0.1234,
        };
        let p = build_prompt(&s);
        assert!(p.contains("Analyses run: 42"));
        assert!(p.contains("Fixes applied: 5 (4 succeeded)"));
        assert!(p.contains("App updates attempted: 3 (3 succeeded)"));
        assert!(p.contains("Patterns learned about this machine: 7"));
        assert!(p.contains("$0.12"));
        // A short instruction, not a giant prompt.
        assert!(p.len() < 1200);
    }
}
