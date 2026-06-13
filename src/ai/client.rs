use crate::models::{ClaudeDecision, SignalSnapshot, PastDecision};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    delta: Option<Delta>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
}

pub struct AiClient {
    http: Client,
    api_key: String,
    model: String,
}

impl AiClient {
    pub fn new(api_key: &str, model: &str) -> Self {
        Self {
            http: Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    pub async fn analyze(
        &self,
        snapshot: &SignalSnapshot,
        history: &[PastDecision],
    ) -> Result<ClaudeDecision> {
        let prompt = crate::ai::prompt::build(snapshot, history);

        let request = ApiRequest {
            model: &self.model,
            max_tokens: 4096,
            stream: true,
            messages: vec![Message {
                role: "user",
                content: &prompt,
            }],
        };

        let response = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("Anthropic API returned {status}: {body}");
        }

        let mut text = String::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Stream read error")?;
            let raw = std::str::from_utf8(&chunk).unwrap_or("");

            for line in raw.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    if let Ok(event) = serde_json::from_str::<StreamEvent>(data) {
                        if event.event_type == "content_block_delta" {
                            if let Some(delta) = event.delta {
                                if delta.delta_type.as_deref() == Some("text_delta") {
                                    if let Some(t) = delta.text {
                                        text.push_str(&t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        debug!("Raw Claude response: {}", &text[..text.len().min(500)]);

        // Strip markdown code fences if present
        let json_text = if let Some(start) = text.find("```json") {
            let after = &text[start + 7..];
            after
                .find("```")
                .map(|end| &after[..end])
                .unwrap_or(after)
                .trim()
        } else if let Some(start) = text.find("```") {
            let after = &text[start + 3..];
            after
                .find("```")
                .map(|end| &after[..end])
                .unwrap_or(after)
                .trim()
        } else {
            text.trim()
        };

        let decision: ClaudeDecision = serde_json::from_str(json_text)
            .with_context(|| format!("Failed to parse Claude response as JSON: {json_text}"))?;

        info!(
            problems = decision.problems.len(),
            analysis = %decision.analysis,
            "Claude analysis complete"
        );

        Ok(decision)
    }
}
