use crate::config::{ApiConfig, ApiProvider};
use crate::models::{ClaudeDecision, PastDecision, SignalSnapshot};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ── Anthropic native request/response ────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct AnthropicEvent {
    #[serde(rename = "type")]
    event_type: String,
    delta: Option<AnthropicDelta>,
}

#[derive(Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
}

// ── OpenAI-compatible request/response (claude-max-api-proxy) ─────────────────

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Deserialize)]
struct OpenAiChunk {
    choices: Option<Vec<OpenAiChoice>>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
}

// ── Client ────────────────────────────────────────────────────────────────────

pub struct AiClient {
    http: Client,
    config: AiClientConfig,
}

enum AiClientConfig {
    Anthropic {
        api_key: String,
        model: String,
    },
    OpenAiCompatible {
        base_url: String,
        api_key: String,
        model: String,
    },
}

impl AiClient {
    pub fn new(cfg: &ApiConfig) -> Result<Self> {
        let inner =
            match cfg.provider {
                ApiProvider::Anthropic => {
                    let key = cfg.anthropic_api_key.clone().context(
                        "[api] anthropic_api_key is required for provider = \"anthropic\"",
                    )?;
                    AiClientConfig::Anthropic {
                        api_key: key,
                        model: cfg.model.clone(),
                    }
                }
                ApiProvider::OpenAiCompatible => {
                    let base_url = cfg.base_url.clone().context(
                        "[api] base_url is required for provider = \"openai_compatible\"",
                    )?;
                    let api_key = cfg.api_key.clone().unwrap_or_else(|| "not-needed".into());
                    AiClientConfig::OpenAiCompatible {
                        base_url: base_url.trim_end_matches('/').to_string(),
                        api_key,
                        model: cfg.model.clone(),
                    }
                }
            };
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            config: inner,
        })
    }

    pub async fn analyze(
        &self,
        snapshot: &SignalSnapshot,
        history: &[PastDecision],
        feedback_summary: Option<&str>,
    ) -> Result<ClaudeDecision> {
        let prompt = crate::ai::prompt::build(snapshot, history, feedback_summary);
        let raw = match &self.config {
            AiClientConfig::Anthropic { api_key, model } => {
                self.call_anthropic(api_key, model, &prompt).await?
            }
            AiClientConfig::OpenAiCompatible {
                base_url,
                api_key,
                model,
            } => {
                self.call_openai_compatible(base_url, api_key, model, &prompt)
                    .await?
            }
        };

        let json_text = strip_fences(&raw);
        debug!(
            text = &json_text[..json_text.len().min(500)],
            "Raw Claude response"
        );

        let decision: ClaudeDecision = serde_json::from_str(json_text)
            .with_context(|| format!("Failed to parse Claude response as JSON:\n{json_text}"))?;

        info!(
            problems = decision.problems.len(),
            analysis = %decision.analysis,
            "Claude analysis complete"
        );

        Ok(decision)
    }

    // ── Anthropic native (/v1/messages) ──────────────────────────────────────

    async fn call_anthropic(&self, api_key: &str, model: &str, prompt: &str) -> Result<String> {
        let body = AnthropicRequest {
            model,
            max_tokens: 4096,
            stream: true,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Anthropic API request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("Anthropic API {status}: {text}");
        }

        let mut out = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Anthropic stream read error")?;
            for line in std::str::from_utf8(&chunk).unwrap_or("").lines() {
                if let Some(data) = line.trim().strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    if let Ok(ev) = serde_json::from_str::<AnthropicEvent>(data) {
                        if ev.event_type == "content_block_delta" {
                            if let Some(d) = ev.delta {
                                if d.delta_type.as_deref() == Some("text_delta") {
                                    if let Some(t) = d.text {
                                        out.push_str(&t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    // ── OpenAI-compatible (/v1/chat/completions, claude-max-api-proxy) ────────

    async fn call_openai_compatible(
        &self,
        base_url: &str,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<String> {
        let url = format!("{base_url}/chat/completions");
        let body = OpenAiRequest {
            model,
            max_tokens: 4096,
            stream: true,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "OpenAI-compatible request to {url} failed — is claude-max-api-proxy running?"
                )
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("OpenAI-compatible API {status}: {text}");
        }

        let mut out = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("OpenAI-compatible stream read error")?;
            for line in std::str::from_utf8(&chunk).unwrap_or("").lines() {
                let line = line.trim();
                // The proxy sends ":ok" as a heartbeat — skip it
                if line.starts_with(':') {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    if let Ok(chunk) = serde_json::from_str::<OpenAiChunk>(data) {
                        if let Some(choices) = chunk.choices {
                            if let Some(choice) = choices.into_iter().next() {
                                if let Some(content) = choice.delta.content {
                                    out.push_str(&content);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

fn strip_fences(s: &str) -> &str {
    if let Some(start) = s.find("```json") {
        let after = &s[start + 7..];
        return after
            .find("```")
            .map(|e| &after[..e])
            .unwrap_or(after)
            .trim();
    }
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        return after
            .find("```")
            .map(|e| &after[..e])
            .unwrap_or(after)
            .trim();
    }
    s.trim()
}
