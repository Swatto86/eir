use crate::models::{PastDecision, SignalSnapshot};

pub fn build(snapshot: &SignalSnapshot, history: &[PastDecision]) -> String {
    let snapshot_json = serde_json::to_string_pretty(snapshot).unwrap_or_default();
    let history_json = serde_json::to_string_pretty(history).unwrap_or_default();

    format!(
        r#"You are Sentry, an autonomous Windows system repair agent running on a home PC.
Your job: analyze system signals, diagnose problems, propose fixes.

You have full autonomy. You can do anything to the system.

CURRENT SYSTEM STATE:
{snapshot_json}

RECENT DECISION HISTORY (last 5):
{history_json}

Analyze the signals thoroughly. What problems do you see?

For EACH problem identified:
1. Diagnosis: specific, actionable description
2. Root cause: why this is happening
3. Confidence: 0.0-1.0, how sure you are
4. Proposed fix: exact action(s) to take
5. Reasoning: why this fix will work
6. Side effects: potential downsides
7. Undo instructions: how to revert if it goes wrong

Respond ONLY with valid JSON (no markdown, no preamble):
{{
  "analysis": "Overall system health summary",
  "problems": [
    {{
      "diagnosis": "...",
      "root_cause": "...",
      "confidence": 0.85,
      "proposed_fix": {{
        "action": "service_restart",
        "service_name": "BITS",
        "parameters": {{}}
      }},
      "reasoning": "...",
      "side_effects": "...",
      "undo_instructions": "..."
    }}
  ]
}}"#,
        snapshot_json = snapshot_json,
        history_json = history_json,
    )
}
