//! Recover structured JSON from model replies that cheaper / free models often
//! wrap, repeat, comment, or slightly invalidate.
//!
//! The analysis cycle, updater parsers, and startup classifier all go through
//! this module so a trailing comma or a `<think>` block cannot fail a whole run
//! when a complete payload is sitting in the same reply.

use crate::models::{ClaudeDecision, Problem};
use serde::de::DeserializeOwned;
use serde_json::Value;

const THINK_TAGS: &[(&str, &str)] = &[
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];

const ACTION_FIELDS: &[&str] = &[
    "action",
    "service_name",
    "path",
    "days_old",
    "target",
    "script",
    "task_name",
    "key_path",
    "value_name",
    "value_data",
    "command",
    "driver_name",
    "package_name",
    "element",
    "value",
    "process_name",
    "profile",
    "name",
    "location",
    "hive",
    "enable",
];

/// Pull the first complete JSON object out of a reply that may wrap it in prose
/// or code fences. Brace-matched (string- and escape-aware) rather than
/// first-`{` to last-`}`: cheaper free models routinely emit the object twice.
pub(crate) fn extract_json(s: &str) -> &str {
    extract_delimited(strip_fences(s), '{', '}').unwrap_or(strip_fences(s))
}

/// First complete `[`…`]` array, or `None` when the reply has no array.
pub(crate) fn extract_json_array(s: &str) -> Option<&str> {
    extract_delimited(s, '[', ']')
}

/// Deserialize `T` from a model reply, recovering from fences, thinking tags,
/// trailing commas, comments, a repeated value, or wrapping prose.
pub(crate) fn parse_model_json<T: DeserializeOwned>(s: &str) -> Result<T, serde_json::Error> {
    parse_model_json_matching(s, |_| true)
}

/// Like [`parse_model_json`], but skips a recovered JSON value that does not
/// satisfy `pred` so a prose `{error}` token is not taken over a later payload.
pub(crate) fn parse_model_json_matching<T, F>(s: &str, pred: F) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
    F: Fn(&Value) -> bool,
{
    let prepared = prepare_model_text(s);
    let mut last_err: Option<serde_json::Error> = None;
    for candidate in json_candidates(&prepared) {
        let sanitized = sanitize_json(candidate);
        match serde_json::from_str::<Value>(&sanitized) {
            Ok(value) if pred(&value) => match serde_json::from_value::<T>(value) {
                Ok(parsed) => return Ok(parsed),
                Err(err) => last_err = Some(err),
            },
            Ok(_) => {}
            Err(err) => last_err = Some(err),
        }
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    serde_json::from_str(&sanitize_json(&prepared))
}

/// Recover a [`ClaudeDecision`] from a free-model reply. Accepts a wrapped
/// object, a bare problem, a bare problems array, flattened action fields, and
/// the usual fence / trailing-comma / think-tag noise.
pub(crate) fn parse_decision(s: &str) -> Result<ClaudeDecision, serde_json::Error> {
    let prepared = prepare_model_text(s);
    let mut last_err: Option<serde_json::Error> = None;
    for candidate in json_candidates(&prepared) {
        let sanitized = sanitize_json(candidate);
        match serde_json::from_str::<Value>(&sanitized) {
            Ok(value) => match decision_from_value(value) {
                Ok(decision) => return Ok(decision),
                Err(err) => last_err = Some(err),
            },
            Err(err) => last_err = Some(err),
        }
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    serde_json::from_str(s)
}

fn decision_from_value(value: Value) -> Result<ClaudeDecision, serde_json::Error> {
    if let Some(mut obj) = match value {
        Value::Object(obj) if looks_like_decision(&obj) => Some(Value::Object(obj)),
        Value::Object(obj) if looks_like_problem(&Value::Object(obj.clone())) => {
            return serde_json::from_value(Value::Object(normalize_problem_object(obj))).map(
                |problem: Problem| ClaudeDecision {
                    analysis: String::new(),
                    problems: vec![problem],
                    needs_deeper_analysis: false,
                },
            );
        }
        Value::Array(items) => {
            let problems = items
                .into_iter()
                .map(|item| match item {
                    Value::Object(obj) => serde_json::from_value::<Problem>(Value::Object(
                        normalize_problem_object(obj),
                    )),
                    other => serde_json::from_value::<Problem>(other),
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(ClaudeDecision {
                analysis: String::new(),
                problems,
                needs_deeper_analysis: false,
            });
        }
        _ => None,
    } {
        if let Some(map) = obj.as_object_mut() {
            if let Some(Value::Array(items)) = map.get_mut("problems") {
                for item in items.iter_mut() {
                    if let Value::Object(obj) = item {
                        *item = Value::Object(normalize_problem_object(std::mem::take(obj)));
                    }
                }
            }
        }
        return serde_json::from_value(obj);
    }
    serde_json::from_value(Value::Null)
}

fn looks_like_decision(obj: &serde_json::Map<String, Value>) -> bool {
    obj.contains_key("analysis")
        || obj.contains_key("problems")
        || obj.contains_key("needs_deeper_analysis")
        || obj.contains_key("needsDeeperAnalysis")
}

fn looks_like_problem(value: &Value) -> bool {
    value.get("diagnosis").is_some() || value.get("proposed_fix").is_some()
}

fn normalize_problem_object(
    mut obj: serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    if !obj.contains_key("proposed_fix") && obj.contains_key("action") {
        let mut fix = serde_json::Map::new();
        for key in ACTION_FIELDS {
            if let Some(value) = obj.get(*key).cloned() {
                fix.insert((*key).to_string(), value);
            }
        }
        obj.insert("proposed_fix".into(), Value::Object(fix));
    }
    if let Some(Value::String(raw)) = obj.get("proposed_fix") {
        let trimmed = raw.trim();
        if trimmed.starts_with('{') {
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                obj.insert("proposed_fix".into(), parsed);
            }
        }
    }
    obj
}

fn prepare_model_text(s: &str) -> String {
    strip_fences(&strip_think_tags(s)).to_string()
}

fn strip_think_tags(s: &str) -> String {
    let mut rest = s;
    let mut out = String::with_capacity(s.len());
    loop {
        let next = THINK_TAGS
            .iter()
            .filter_map(|(open, close)| find_ci_ascii(rest, open).map(|i| (i, *open, *close)))
            .min_by_key(|(i, _, _)| *i);
        let Some((i, open, close)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..i]);
        let after_open = &rest[i + open.len()..];
        match find_ci_ascii(after_open, close) {
            Some(j) => rest = &after_open[j + close.len()..],
            None => {
                // Unclosed tag: keep the remainder so a JSON object after it still parses.
                out.push_str(&rest[i..]);
                break;
            }
        }
    }
    out
}

fn find_ci_ascii(hay: &str, needle: &str) -> Option<usize> {
    let hay_bytes = hay.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || hay_bytes.len() < needle_bytes.len() {
        return None;
    }
    for i in 0..=hay_bytes.len() - needle_bytes.len() {
        if needle_bytes
            .iter()
            .enumerate()
            .all(|(j, b)| hay_bytes[i + j].eq_ignore_ascii_case(b))
        {
            return Some(i);
        }
    }
    None
}

/// Only a fence at the very START of the (trimmed) response is a real code fence.
/// A triple-backtick run inside a log excerpt the model echoed back must not be
/// mistaken for the opening fence. Language tags (`json`, `JSON`, `javascript`)
/// are ignored.
pub(crate) fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    let (rest, close) = if let Some(rest) = t.strip_prefix("```") {
        (rest, "```")
    } else if let Some(rest) = t.strip_prefix("~~~") {
        (rest, "~~~")
    } else {
        return t;
    };
    let rest = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest
            .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-')
            .trim_start(),
    };
    rest.find(close)
        .map(|end| rest[..end].trim())
        .unwrap_or(rest.trim())
}

fn json_candidates(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < s.len() {
        let rest = &s[pos..];
        let Some(rel) = rest.find(['{', '[']) else {
            break;
        };
        let start = pos + rel;
        match close_delimited_at(s, start) {
            Some(end) => {
                out.push(&s[start..=end]);
                pos = end + 1;
            }
            None => {
                let closer = if s.as_bytes().get(start) == Some(&b'[') {
                    ']'
                } else {
                    '}'
                };
                let span = match s[start..].rfind(closer) {
                    Some(rel) if rel > 0 => &s[start..=start + rel],
                    _ => &s[start..],
                };
                out.push(span);
                break;
            }
        }
    }
    out
}

fn extract_delimited(s: &str, open: char, close: char) -> Option<&str> {
    let start = s.find(open)?;
    match close_delimited_at(s, start) {
        Some(end) => Some(&s[start..=end]),
        None => match s.rfind(close) {
            Some(end) if end > start => Some(&s[start..=end]),
            _ => Some(&s[start..]),
        },
    }
}

fn close_delimited_at(s: &str, start: usize) -> Option<usize> {
    let open = s[start..].chars().next()?;
    let close = match open {
        '{' => '}',
        '[' => ']',
        _ => return None,
    };
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    for (i, c) in s[start..].char_indices() {
        if escaped {
            escaped = false;
        } else if in_str {
            match c {
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                '"' => in_str = true,
                c if c == open => depth += 1,
                c if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(start + i);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Repair the JSON-ish slips cheaper models make: smart quotes, `//` / `/* */`
/// comments, trailing commas, and raw control characters inside strings.
fn sanitize_json(input: &str) -> String {
    let s = input.trim().trim_start_matches('\u{feff}');
    let s = s.replace(['\u{201c}', '\u{201d}'], "\"");
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    let mut escaped = false;
    while i < chars.len() {
        let c = chars[i];
        if escaped {
            out.push(c);
            escaped = false;
            i += 1;
            continue;
        }
        if in_str {
            match c {
                '\\' => {
                    out.push(c);
                    escaped = true;
                }
                '"' => {
                    out.push(c);
                    in_str = false;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {}
                _ => out.push(c),
            }
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = i.saturating_add(2).min(chars.len());
            continue;
        }
        if c == ',' {
            let j = skip_ws_and_comments(&chars, i + 1);
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        if c == '"' {
            in_str = true;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn skip_ws_and_comments(chars: &[char], mut i: usize) -> usize {
    loop {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = i.saturating_add(2).min(chars.len());
            continue;
        }
        break;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_and_extract_json() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("```JSON\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("noise {\"a\":1} trailing"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_ignores_backticks_inside_the_payload() {
        let raw = "{\"diagnosis\":\"saw ```\\nboom\\n``` in the log\",\"confidence\":0.9}";
        assert_eq!(strip_fences(raw), raw);
        let wrapped = "Here is the answer:\n```\n{\"diagnosis\":\"x\"}\n```";
        assert_eq!(extract_json(wrapped), "{\"diagnosis\":\"x\"}");
    }

    #[test]
    fn extract_json_takes_the_first_complete_object() {
        // Observed with a cheap free model: it emitted the verdict, then began repeating
        // it, so the response was `{obj}{partial`. First-`{`..last-`}` spanned both and
        // parsed as nothing; the first complete object must be recovered instead.
        let doubled = "{\"analysis\":\"healthy\",\"problems\":[]}{\"analysis\":\"heal";
        assert_eq!(
            extract_json(doubled),
            "{\"analysis\":\"healthy\",\"problems\":[]}"
        );
        serde_json::from_str::<serde_json::Value>(extract_json(doubled)).unwrap();

        let nested = "prose {\"a\":{\"b\":\"}\"},\"c\":\"\\\\\"} tail {\"x\":1}";
        assert_eq!(extract_json(nested), "{\"a\":{\"b\":\"}\"},\"c\":\"\\\\\"}");
        serde_json::from_str::<serde_json::Value>(extract_json(nested)).unwrap();

        let truncated = "{\"analysis\":\"heal";
        assert_eq!(extract_json(truncated), truncated);
        assert!(serde_json::from_str::<serde_json::Value>(extract_json(truncated)).is_err());
    }

    #[test]
    fn extract_json_array_takes_the_first_complete_array() {
        assert_eq!(extract_json_array("noise [1,2] tail"), Some("[1,2]"));
        assert_eq!(extract_json_array("no array here"), None);
        // Same doubled-value bug as objects: first-`[`..last-`]` spanned both.
        assert_eq!(extract_json_array("[1,2][3"), Some("[1,2]"));
    }

    #[test]
    fn parse_decision_recovers_free_model_slips() {
        let trailing = r#"{
            "analysis": "ok",
            "needs_deeper_analysis": false,
            "problems": [],
        }"#;
        let d = parse_decision(trailing).expect("trailing comma");
        assert_eq!(d.analysis, "ok");
        assert!(d.problems.is_empty());

        let think = "<think>I will emit JSON {\"analysis\":\"nope\"} now</think>\n{\"analysis\":\"healthy\",\"problems\":[]}";
        let d = parse_decision(think).expect("think tags");
        assert_eq!(d.analysis, "healthy");

        let comment = "{\"analysis\":\"ok\", // brief\n\"problems\": []}";
        let d = parse_decision(comment).expect("comment");
        assert_eq!(d.analysis, "ok");

        let missing = r#"{"analysis":"fine"}"#;
        let d = parse_decision(missing).expect("missing problems");
        assert!(d.problems.is_empty());

        let nulls = r#"{"analysis":null,"problems":null,"needs_deeper_analysis":null}"#;
        let d = parse_decision(nulls).expect("null fields");
        assert!(d.analysis.is_empty());
        assert!(d.problems.is_empty());
        assert!(!d.needs_deeper_analysis);

        let newline = "{\"analysis\":\"line1\nline2\",\"problems\":[]}";
        let d = parse_decision(newline).expect("raw newline in string");
        assert!(d.analysis.contains("line1"));
        assert!(d.analysis.contains("line2"));
    }

    #[test]
    fn parse_decision_wraps_a_bare_problem_or_array() {
        let one = r#"{"diagnosis":"spooler stopped","proposed_fix":{"action":"service_start","service_name":"Spooler"},"confidence":0.9}"#;
        let d = parse_decision(one).expect("bare problem");
        assert_eq!(d.problems.len(), 1);
        assert_eq!(d.problems[0].diagnosis, "spooler stopped");

        let arr = r#"[{"diagnosis":"a","proposed_fix":{"action":"sfc_scan"},"confidence":"90%"}]"#;
        let d = parse_decision(arr).expect("bare array");
        assert_eq!(d.problems.len(), 1);
        assert!((d.problems[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_decision_lifts_a_flattened_action() {
        let flat = r#"{
            "analysis": "fix spooler",
            "problems": [{
                "diagnosis": "spooler stopped",
                "action": "service_start",
                "service_name": "Spooler",
                "confidence": 90
            }]
        }"#;
        let d = parse_decision(flat).expect("flattened action");
        match d.problems[0].parse_fix_action() {
            Some(crate::models::FixAction::ServiceStart { service_name }) => {
                assert_eq!(service_name, "Spooler");
            }
            other => panic!("expected service_start, got {other:?}"),
        }
    }

    #[test]
    fn parse_decision_skips_a_prose_brace_then_the_payload() {
        let prose = "The {error} field is normal. {\"analysis\":\"healthy\",\"problems\":[]}";
        let d = parse_decision(prose).expect("prose brace");
        assert_eq!(d.analysis, "healthy");
    }

    #[test]
    fn parse_decision_does_not_treat_an_error_object_as_healthy() {
        let err = r#"{"error":"rate limited"}"#;
        assert!(parse_decision(err).is_err());
    }

    #[test]
    fn parse_decision_accepts_string_bool_and_percent_confidence() {
        let raw = r#"{
            "analysis": "ok",
            "needs_deeper_analysis": "true",
            "problems": [{
                "diagnosis": "x",
                "proposed_fix": {"action": "sfc_scan"},
                "confidence": "80%"
            }]
        }"#;
        let d = parse_decision(raw).expect("lenient scalars");
        assert!(d.needs_deeper_analysis);
        assert!((d.problems[0].confidence - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_model_json_recovers_updater_shapes() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Plan {
            installer_url: String,
        }
        let fenced = "Here's the installer:\n```json\n{\"installer_url\":\"https://example.com/app.exe\",}\n```\n";
        let plan: Plan = parse_model_json(fenced).expect("fenced plan");
        assert_eq!(plan.installer_url, "https://example.com/app.exe");
    }

    #[test]
    fn close_delimited_counts_only_the_opening_kind() {
        let s = "[{\"a\":[1,2]},3]";
        assert_eq!(extract_json_array(s), Some(s));
    }
}
