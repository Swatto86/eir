// Phase 1: no execution. This module exists as a placeholder.
// Phase 2 will add executor/services.rs, executor/logs.rs, executor/powershell.rs.
use crate::models::ClaudeDecision;
use tracing::info;

pub fn log_proposed(decision: &ClaudeDecision) {
    for (i, problem) in decision.problems.iter().enumerate() {
        info!(
            index = i + 1,
            confidence = problem.confidence,
            diagnosis = %problem.diagnosis,
            root_cause = %problem.root_cause,
            proposed_fix = %problem.proposed_fix,
            "PROPOSED FIX (not executing — Phase 1)"
        );
    }
}
