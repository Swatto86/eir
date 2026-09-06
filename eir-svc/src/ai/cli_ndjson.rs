//! Shared NDJSON parser for OpenCode (identical wire shape to the former Kilo CLI).

use crate::models::CallUsage;
use anyhow::Result;
use serde_json::Value;

pub(crate) fn provider_cost(cost: f64) -> f64 {
    if cost.is_finite() && cost >= 0.0 {
        cost
    } else {
        0.0
    }
}

/// Parse OpenCode / Kilo-style `--format json` NDJSON.
///
/// Events nest payload under `part` (e.g. `{"type":"text","part":{"text":"..."}}`);
/// top-level fallbacks cover older variants. Concatenate `text` events; keep the
/// latest `step_finish` token/cost accounting.
pub(crate) fn parse_agent_ndjson(stdout: &str) -> Result<(String, Option<CallUsage>)> {
    let mut text = String::new();
    let mut last_step: Option<Value> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev["type"].as_str() {
            Some("text") => {
                let t = ev["part"]["text"].as_str().or_else(|| ev["text"].as_str());
                if let Some(t) = t {
                    text.push_str(t);
                }
            }
            Some("step_finish") => last_step = Some(ev),
            _ => {}
        }
    }
    let usage = last_step.map(|s| {
        let part_tokens = &s["part"]["tokens"];
        let tokens = if part_tokens.is_null() {
            &s["tokens"]
        } else {
            part_tokens
        };
        let input = tokens["input"]
            .as_u64()
            .unwrap_or_else(|| tokens["input_tokens"].as_u64().unwrap_or(0));
        let output = tokens["output"]
            .as_u64()
            .unwrap_or_else(|| tokens["output_tokens"].as_u64().unwrap_or(0));
        let cache_read = tokens["cache"]["read"]
            .as_u64()
            .unwrap_or_else(|| tokens["cache_read_input_tokens"].as_u64().unwrap_or(0));
        let cache_write = tokens["cache"]["write"]
            .as_u64()
            .unwrap_or_else(|| tokens["cache_creation_input_tokens"].as_u64().unwrap_or(0));
        let cost = s["part"]["cost"]
            .as_f64()
            .unwrap_or_else(|| s["cost"].as_f64().unwrap_or(0.0));
        CallUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation: cache_write,
            cache_read,
            cost_usd: provider_cost(cost),
        }
    });
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ndjson_collects_text_and_keeps_last_step_usage() {
        let stream = r#"
{"type":"step_start","sessionID":"abc","part":{"type":"step-start"}}
{"type":"text","part":{"type":"text","text":"hello "}}
{"type":"text","part":{"type":"text","text":"world"}}
{"type":"step_finish","part":{"type":"step-finish","cost":0.001,"tokens":{"input":100,"output":20,"cache":{"read":0,"write":50}}}}
{"type":"text","part":{"type":"text","text":"!"}}
{"type":"step_finish","part":{"type":"step-finish","cost":0.002,"tokens":{"input":150,"output":25,"cache":{"read":10,"write":50}}}}
{"type":"session_end"}
"#;
        let (text, usage) = parse_agent_ndjson(stream).unwrap();
        assert_eq!(text, "hello world!");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 150);
        assert_eq!(u.output_tokens, 25);
        assert_eq!(u.cache_read, 10);
        assert_eq!(u.cache_creation, 50);
        assert!((u.cost_usd - 0.002).abs() < 1e-9);
    }

    #[test]
    fn agent_ndjson_tolerates_garbage_lines_and_alternate_token_shape() {
        let stream = "Some log preamble\n[stderr-redir] debug: hi\n\
            {\"type\":\"text\",\"text\":\"ok\"}\n\
            {\"type\":\"step_finish\",\"cost\":0.01,\"tokens\":{\"input_tokens\":7,\"output_tokens\":3,\"cache\":{\"read\":0,\"write\":0}}}\n";
        let (text, usage) = parse_agent_ndjson(stream).unwrap();
        assert_eq!(text, "ok");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
    }
}
