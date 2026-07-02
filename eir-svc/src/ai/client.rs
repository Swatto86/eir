use crate::config::{ApiConfig, ApiProvider};
use crate::models::{CallUsage, ClaudeDecision, PastDecision, SignalSnapshot};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, info};

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
/// Kilo Code AI gateway — OpenAI-compatible `/chat/completions`.
const KILOCODE_BASE: &str = "https://api.kilo.ai/api/gateway";
const MAX_TOKENS: u32 = 4096;

// ── OpenAI-compatible request/response (OpenRouter, Kilo Code) ────────────────

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    /// OpenRouter-specific: request a final usage chunk with cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageInclude>,
    /// Reasoning effort (OpenRouter / Kilo Code reasoning models).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct UsageInclude {
    include: bool,
}

#[derive(Serialize)]
struct Reasoning<'a> {
    effort: &'a str,
}

#[derive(Deserialize)]
struct OpenAiChunk {
    choices: Option<Vec<OpenAiChoice>>,
    /// Providers may stream an error object instead of content.
    error: Option<OpenAiStreamError>,
    /// Present in the final chunk when usage accounting is requested.
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    /// OpenRouter adds the request cost (USD); 0 for free models.
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct OpenAiStreamError {
    message: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
}

/// Which OpenAI-compatible backend a call targets (they differ only in the
/// extra headers/fields they want).
#[derive(Clone, Copy, PartialEq)]
enum OpenAiFlavor {
    OpenRouter,
    KiloCode,
}

/// Accumulates raw SSE bytes and yields complete lines. Byte-based on purpose:
/// converting each network chunk to &str would silently drop a whole chunk when
/// a multi-byte UTF-8 character straddles a chunk boundary.
struct SseLineBuf {
    buf: Vec<u8>,
}

impl SseLineBuf {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Next complete, trimmed line — or None until one arrives.
    fn next_line(&mut self) -> Option<String> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = self.buf.drain(..=pos).collect();
        Some(String::from_utf8_lossy(&line).trim().to_string())
    }
}

// ── Client ────────────────────────────────────────────────────────────────────

pub struct AiClient {
    http: Client,
    config: AiClientConfig,
    /// Reasoning effort (low|medium|high|xhigh|max, validated upstream; empty =
    /// provider default). Applied per provider in the call functions.
    effort: String,
}

enum AiClientConfig {
    Anthropic { api_key: String, model: String },
    OpenRouter { api_key: String, model: String },
    KiloCode { api_key: String, model: String },
}

impl AiClient {
    pub fn new(cfg: &ApiConfig) -> Result<Self> {
        let inner = match cfg.provider {
            ApiProvider::Anthropic => {
                let key = cfg
                    .anthropic_api_key
                    .clone()
                    .filter(|k| !k.trim().is_empty())
                    .context("[api] anthropic_api_key is required for provider = \"anthropic\"")?;
                if cfg.model.trim().is_empty() {
                    bail!("[api] a model is required for provider = \"anthropic\" (e.g. claude-opus-4-8 or claude-haiku-4-5)");
                }
                AiClientConfig::Anthropic {
                    api_key: key,
                    model: cfg.model.clone(),
                }
            }
            ApiProvider::OpenRouter => {
                // Use the configured key, else auto-detect one from a logged-in
                // OpenRouter CLI (~/.openrouter/config.json) — no key pasting needed.
                let api_key = cfg
                    .openrouter_api_key
                    .clone()
                    .filter(|k| !k.trim().is_empty())
                    .or_else(resolve_openrouter_key)
                    .context(
                        "[api] OpenRouter needs an API key — add it in Settings, or log in with the OpenRouter CLI (~/.openrouter/config.json)",
                    )?;
                // Blank model defaults to the free auto-routing meta-model.
                let model = if cfg.model.trim().is_empty() {
                    "openrouter/free".to_string()
                } else {
                    cfg.model.clone()
                };
                AiClientConfig::OpenRouter { api_key, model }
            }
            ApiProvider::KiloCode => {
                let api_key = cfg
                    .kilocode_api_key
                    .clone()
                    .filter(|k| !k.trim().is_empty())
                    .context("[api] kilocode_api_key is required for provider = \"kilocode\" — create one at app.kilo.ai")?;
                if cfg.model.trim().is_empty() {
                    bail!("[api] a model is required for provider = \"kilocode\" (e.g. anthropic/claude-sonnet-4.6)");
                }
                AiClientConfig::KiloCode {
                    api_key,
                    model: cfg.model.clone(),
                }
            }
        };
        Ok(Self {
            // Generous timeout: free OpenRouter models can take 60s+ to respond.
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?,
            config: inner,
            effort: cfg.effort.clone(),
        })
    }

    pub async fn analyze(
        &self,
        snapshot: &SignalSnapshot,
        history: &[PastDecision],
        feedback_summary: Option<&str>,
        learned: Option<&str>,
    ) -> Result<(ClaudeDecision, Option<CallUsage>)> {
        self.analyze_with(snapshot, history, feedback_summary, learned, None, None)
            .await
    }

    /// As [`analyze`], but with an optional per-call model and/or reasoning-effort
    /// override — the advisor-mode escalation lever. Both overrides apply to every
    /// provider; a `None` (or empty) override keeps the configured value.
    #[allow(clippy::too_many_arguments)]
    pub async fn analyze_with(
        &self,
        snapshot: &SignalSnapshot,
        history: &[PastDecision],
        feedback_summary: Option<&str>,
        learned: Option<&str>,
        model_override: Option<&str>,
        effort_override: Option<&str>,
    ) -> Result<(ClaudeDecision, Option<CallUsage>)> {
        let model_ov = model_override.map(str::trim).filter(|s| !s.is_empty());
        let effort = effort_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.effort);
        let prompt = crate::ai::prompt::build(snapshot, history, feedback_summary, learned);
        let (raw, usage) = match &self.config {
            AiClientConfig::Anthropic { api_key, model } => {
                let m = model_ov.unwrap_or(model);
                self.call_anthropic(api_key, m, effort, &prompt).await?
            }
            AiClientConfig::OpenRouter { api_key, model } => {
                let m = model_ov.unwrap_or(model);
                self.call_openai_style(
                    OPENROUTER_BASE,
                    api_key,
                    m,
                    effort,
                    &prompt,
                    OpenAiFlavor::OpenRouter,
                )
                .await?
            }
            AiClientConfig::KiloCode { api_key, model } => {
                let m = model_ov.unwrap_or(model);
                self.call_openai_style(
                    KILOCODE_BASE,
                    api_key,
                    m,
                    effort,
                    &prompt,
                    OpenAiFlavor::KiloCode,
                )
                .await?
            }
        };

        let json_text = strip_fences(&raw);
        debug!(
            text = &json_text[..json_text.len().min(500)],
            "Raw model response"
        );

        // Models occasionally wrap the JSON in prose; fall back to the
        // first {...last} object if a direct parse fails.
        let decision: ClaudeDecision = match serde_json::from_str(json_text) {
            Ok(d) => d,
            Err(_) => {
                let extracted = extract_json_object(json_text);
                serde_json::from_str(extracted).with_context(|| {
                    format!("Failed to parse model response as JSON:\n{json_text}")
                })?
            }
        };

        info!(
            problems = decision.problems.len(),
            analysis = %decision.analysis,
            "Claude analysis complete"
        );

        Ok((decision, usage))
    }

    // ── Anthropic native (/v1/messages, streaming SSE) ────────────────────────

    async fn call_anthropic(
        &self,
        api_key: &str,
        model: &str,
        effort: &str,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let mut body = json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "stream": true,
            "messages": [{"role": "user", "content": prompt}],
        });
        // GA effort dial (low..max). Haiku models have no effort support and
        // reject the parameter outright, so skip it there rather than turning
        // every analysis cycle into a 400.
        if !effort.is_empty() && !model.to_lowercase().contains("haiku") {
            body["output_config"] = json!({ "effort": effort });
        }

        let resp = self
            .http
            .post(ANTHROPIC_MESSAGES_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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
        let mut lines = SseLineBuf::new();
        let mut done = false;
        let mut stream = resp.bytes_stream();
        // Usage accumulates across the stream: input/cache tokens arrive in
        // message_start, output tokens in the final message_delta.
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cache_creation = 0u64;
        let mut cache_read = 0u64;

        while let Some(chunk) = stream.next().await {
            if done {
                break;
            }
            let chunk = chunk.context("Anthropic stream read error")?;
            lines.push(&chunk);
            while let Some(line) = lines.next_line() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                match ev["type"].as_str() {
                    Some("content_block_delta") => {
                        if ev["delta"]["type"].as_str() == Some("text_delta") {
                            out.push_str(ev["delta"]["text"].as_str().unwrap_or(""));
                        }
                    }
                    Some("message_start") => {
                        let u = &ev["message"]["usage"];
                        input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                        cache_creation = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                        cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    }
                    Some("message_delta") => {
                        if let Some(o) = ev["usage"]["output_tokens"].as_u64() {
                            output_tokens = o;
                        }
                    }
                    Some("error") => {
                        let msg = ev["error"]["message"].as_str().unwrap_or("unknown error");
                        bail!("Anthropic stream error: {msg}");
                    }
                    _ => {}
                }
            }
        }
        if out.trim().is_empty() {
            bail!("Anthropic returned an empty response");
        }
        let usage = CallUsage {
            input_tokens,
            output_tokens,
            cache_creation,
            cache_read,
            cost_usd: estimate_anthropic_cost(
                model,
                input_tokens,
                output_tokens,
                cache_creation,
                cache_read,
                0,
            ),
        };
        Ok((out, Some(usage)))
    }

    // ── OpenAI-compatible streaming (/chat/completions) — OpenRouter & Kilo ───

    async fn call_openai_style(
        &self,
        base_url: &str,
        api_key: &str,
        model: &str,
        effort: &str,
        prompt: &str,
        flavor: OpenAiFlavor,
    ) -> Result<(String, Option<CallUsage>)> {
        let url = format!("{base_url}/chat/completions");
        let body = OpenAiRequest {
            model,
            max_tokens: MAX_TOKENS,
            stream: true,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
            // Ask for a final usage chunk (tokens + cost where supported).
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            usage: (flavor == OpenAiFlavor::OpenRouter).then_some(UsageInclude { include: true }),
            reasoning: openai_effort(effort).map(|e| Reasoning { effort: e }),
        };

        let mut req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json");
        if flavor == OpenAiFlavor::OpenRouter {
            // OpenRouter app attribution (optional but recommended).
            req = req
                .header("HTTP-Referer", "https://github.com/Swatto86/eir")
                .header("X-Title", "Eir");
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Request to {url} failed"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("model API {status}: {text}");
        }

        let mut out = String::new();
        let mut usage: Option<CallUsage> = None;
        let mut lines = SseLineBuf::new();
        let mut done = false;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            if done {
                break;
            }
            let chunk = chunk.context("stream read error")?;
            lines.push(&chunk);
            while let Some(line) = lines.next_line() {
                // SSE comments (": OPENROUTER PROCESSING" heartbeats) — skip.
                if line.starts_with(':') {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        done = true;
                        break;
                    }
                    if let Ok(chunk) = serde_json::from_str::<OpenAiChunk>(data) {
                        // Surface a streamed error (e.g. free-tier rate limit)
                        // instead of returning an opaque empty response.
                        if let Some(err) = chunk.error {
                            let msg = err.message.unwrap_or_else(|| "unknown error".into());
                            bail!("model API error: {msg}");
                        }
                        if let Some(u) = chunk.usage {
                            usage = Some(CallUsage {
                                input_tokens: u.prompt_tokens.unwrap_or(0),
                                output_tokens: u.completion_tokens.unwrap_or(0),
                                cache_creation: 0,
                                cache_read: 0,
                                cost_usd: u.cost.unwrap_or(0.0),
                            });
                        }
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
        if out.trim().is_empty() {
            bail!("model returned an empty response (it may be rate-limited or unavailable)");
        }
        Ok((out, usage))
    }
}

// ── Web-search completion (app-update plan / diagnosis) ───────────────────────
//
// A general "ask the model this prompt, with live web search where available"
// entry point, used by the updater to resolve an official installer URL and to
// read failure errors. OpenRouter uses its `web` plugin; Anthropic uses the
// native web_search server tool. The Kilo gateway has no verified web-search
// path, so it answers from model knowledge — the updater's plan validator and
// signature gates still bound anything it returns.

#[derive(Serialize)]
struct WebPlugin {
    id: &'static str,
    max_results: u32,
}

#[derive(Serialize)]
struct OpenRouterWebRequest<'a> {
    model: &'a str,
    plugins: Vec<WebPlugin>,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Deserialize)]
struct OrResp {
    #[serde(default)]
    choices: Vec<OrChoice>,
    #[serde(default)]
    usage: Option<OrUsage>,
    #[serde(default)]
    error: Option<OrError>,
}

#[derive(Deserialize)]
struct OrChoice {
    message: OrMsg,
}

#[derive(Deserialize)]
struct OrMsg {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct OrUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct OrError {
    #[serde(default)]
    message: String,
}

impl AiClient {
    /// Ask the configured provider for a completion of `prompt`, with live web
    /// search where the provider supports it. `model_override` (the configured
    /// `update_check_model`) selects the model where it applies. Returns the raw
    /// text plus usage/cost.
    pub async fn complete(
        &self,
        prompt: &str,
        model_override: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let ov = model_override.trim();
        match &self.config {
            AiClientConfig::OpenRouter { api_key, model } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openrouter_web(api_key, m, prompt).await
            }
            AiClientConfig::Anthropic { api_key, .. } => {
                let m = anthropic_web_model(ov);
                self.call_anthropic_web(api_key, &m, prompt).await
            }
            AiClientConfig::KiloCode { api_key, model } => {
                // No verified web plugin on the Kilo gateway — plain completion.
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openai_style(
                    KILOCODE_BASE,
                    api_key,
                    m,
                    "",
                    prompt,
                    OpenAiFlavor::KiloCode,
                )
                .await
            }
        }
    }

    /// Plain text completion with NO web search — for callers that only need a
    /// short answer from the model (e.g. the learned-fact labeller). Cheaper and
    /// bounded: the model can't spend budget on searches it doesn't need.
    pub async fn complete_text(
        &self,
        prompt: &str,
        model_override: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let ov = model_override.trim();
        match &self.config {
            AiClientConfig::Anthropic { api_key, .. } => {
                let m = anthropic_web_model(ov);
                self.call_anthropic(api_key, &m, "", prompt).await
            }
            AiClientConfig::OpenRouter { api_key, model } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openai_style(
                    OPENROUTER_BASE,
                    api_key,
                    m,
                    "",
                    prompt,
                    OpenAiFlavor::OpenRouter,
                )
                .await
            }
            AiClientConfig::KiloCode { api_key, model } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openai_style(
                    KILOCODE_BASE,
                    api_key,
                    m,
                    "",
                    prompt,
                    OpenAiFlavor::KiloCode,
                )
                .await
            }
        }
    }

    /// OpenRouter with its web-search plugin (non-streaming). Works with free
    /// models — OpenRouter performs the search and feeds results to the model.
    async fn call_openrouter_web(
        &self,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let body = OpenRouterWebRequest {
            model,
            plugins: vec![WebPlugin {
                id: "web",
                max_results: 5,
            }],
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
        };
        let resp = self
            .http
            .post(format!("{OPENROUTER_BASE}/chat/completions"))
            .header("Authorization", format!("Bearer {api_key}"))
            .header("HTTP-Referer", "https://github.com/Swatto86/eir")
            .header("X-Title", "Eir")
            .json(&body)
            .send()
            .await
            .context("OpenRouter request failed")?;

        let status = resp.status();
        let text = resp.text().await.context("OpenRouter read failed")?;
        if !status.is_success() {
            let detail: String = text.chars().take(400).collect();
            bail!("OpenRouter error ({status}): {detail}");
        }
        let parsed: OrResp = serde_json::from_str(&text).context("bad OpenRouter response")?;
        if let Some(err) = parsed.error {
            if !err.message.is_empty() {
                bail!("OpenRouter error: {}", err.message);
            }
        }
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        let usage = parsed.usage.map(|u| CallUsage {
            input_tokens: u.prompt_tokens.unwrap_or(0),
            output_tokens: u.completion_tokens.unwrap_or(0),
            cache_creation: 0,
            cache_read: 0,
            cost_usd: u.cost.unwrap_or(0.0),
        });
        Ok((content, usage))
    }

    /// Anthropic with the native web_search server tool (non-streaming). The
    /// basic tool variant works across current Claude models incl. Haiku.
    async fn call_anthropic_web(
        &self,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let body = json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 5}],
            "messages": [{"role": "user", "content": prompt}],
        });
        let resp = self
            .http
            .post(ANTHROPIC_MESSAGES_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Anthropic API request failed")?;

        let status = resp.status();
        let text = resp.text().await.context("Anthropic read failed")?;
        if !status.is_success() {
            let detail: String = text.chars().take(400).collect();
            bail!("Anthropic error ({status}): {detail}");
        }
        let v: Value = serde_json::from_str(&text).context("bad Anthropic response")?;
        let mut content = String::new();
        for block in v["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if block["type"].as_str() == Some("text") {
                content.push_str(block["text"].as_str().unwrap_or(""));
            }
        }
        if content.trim().is_empty() {
            // e.g. a turn that ended on tool use with no text — surface a clear
            // provider error instead of letting the caller fail on JSON parsing.
            bail!(
                "Anthropic web check returned no text (stop_reason: {})",
                v["stop_reason"].as_str().unwrap_or("unknown")
            );
        }
        let u = &v["usage"];
        let input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
        let cache_creation = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let searches = u["server_tool_use"]["web_search_requests"]
            .as_u64()
            .unwrap_or(0);
        let usage = CallUsage {
            input_tokens,
            output_tokens,
            cache_creation,
            cache_read,
            cost_usd: estimate_anthropic_cost(
                model,
                input_tokens,
                output_tokens,
                cache_creation,
                cache_read,
                searches,
            ),
        };
        Ok((content, Some(usage)))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Map Eir's effort levels onto the OpenAI-style `reasoning.effort` dial
/// (low|medium|high). `xhigh`/`max` collapse to `high`; empty = don't send.
fn openai_effort(effort: &str) -> Option<&'static str> {
    match effort {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// Resolve the Claude model for the Anthropic web-search check. The
/// `update_check_model` may be blank (→ cheap Haiku default), a bare alias
/// (haiku/sonnet/opus), or a full claude-* id; anything non-Claude also falls
/// back to Haiku since this path is Anthropic-only.
pub(crate) fn anthropic_web_model(override_model: &str) -> String {
    let m = override_model.trim();
    match m.to_lowercase().as_str() {
        "" => "claude-haiku-4-5".to_string(),
        "haiku" => "claude-haiku-4-5".to_string(),
        "sonnet" => "claude-sonnet-4-6".to_string(),
        "opus" => "claude-opus-4-8".to_string(),
        lower if lower.starts_with("claude") => m.to_string(),
        _ => "claude-haiku-4-5".to_string(),
    }
}

/// Approximate Anthropic pay-as-you-go pricing (USD per million tokens) so the
/// usage display and advisor budget have a cost figure — the API reports token
/// counts but not cost. Prices drift over time; treat as an estimate.
fn anthropic_price_per_mtok(model: &str) -> (f64, f64) {
    let m = model.to_lowercase();
    if m.contains("haiku") {
        (1.0, 5.0)
    } else if m.contains("sonnet") {
        (3.0, 15.0)
    } else if m.contains("fable") || m.contains("mythos") {
        (10.0, 50.0)
    } else {
        (5.0, 25.0) // Opus tier / unknown Claude models
    }
}

/// Estimated cost of one Anthropic call: base tokens at list price, cache
/// writes at 1.25×, cache reads at 0.1×, plus $10/1k web searches.
fn estimate_anthropic_cost(
    model: &str,
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
    web_searches: u64,
) -> f64 {
    let (p_in, p_out) = anthropic_price_per_mtok(model);
    (input as f64 * p_in
        + cache_creation as f64 * p_in * 1.25
        + cache_read as f64 * p_in * 0.1
        + output as f64 * p_out)
        / 1_000_000.0
        + web_searches as f64 * 0.01
}

/// Pull the JSON object out of a model response that may wrap it in prose or code
/// fences (reasoning models often add commentary around the JSON).
pub(crate) fn extract_json(s: &str) -> &str {
    let t = strip_fences(s);
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b > a => &t[a..=b],
        _ => t,
    }
}

/// Scan `C:\Users` for a logged-in OpenRouter CLI config and return its API
/// key. Lets an OpenRouter user run with nothing pasted into Settings.
fn resolve_openrouter_key() -> Option<String> {
    let users = std::fs::read_dir("C:\\Users").ok()?;
    for entry in users.flatten() {
        let path = entry.path().join(".openrouter").join("config.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let key = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("apiKey").and_then(|k| k.as_str()).map(str::to_string))
            .filter(|k| !k.trim().is_empty());
        if let Some(k) = key {
            return Some(k.trim().to_string());
        }
    }
    None
}

/// Extract the outermost JSON object (first `{` to last `}`) — a fallback for
/// models that surround the JSON with prose.
fn extract_json_object(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => &s[a..=b],
        _ => s,
    }
}

fn strip_fences(s: &str) -> &str {
    // Check ````json` before ```` to avoid matching the shorter fence first
    for (open, close) in [
        ("```json", "```"),
        ("```", "```"),
        ("~~~json", "~~~"),
        ("~~~", "~~~"),
    ] {
        if let Some(start) = s.find(open) {
            let after = &s[start + open.len()..];
            return after
                .find(close)
                .map(|e| &after[..e])
                .unwrap_or(after)
                .trim();
        }
    }
    s.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_web_model_resolution() {
        // blank / aliases / non-Claude → sensible Claude ids; claude-* passes through.
        assert_eq!(anthropic_web_model(""), "claude-haiku-4-5");
        assert_eq!(anthropic_web_model("haiku"), "claude-haiku-4-5");
        assert_eq!(anthropic_web_model("sonnet"), "claude-sonnet-4-6");
        assert_eq!(anthropic_web_model("opus"), "claude-opus-4-8");
        assert_eq!(anthropic_web_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(anthropic_web_model("openrouter/free"), "claude-haiku-4-5");
    }

    #[test]
    fn openai_effort_mapping() {
        assert_eq!(openai_effort(""), None);
        assert_eq!(openai_effort("low"), Some("low"));
        assert_eq!(openai_effort("medium"), Some("medium"));
        assert_eq!(openai_effort("high"), Some("high"));
        assert_eq!(openai_effort("xhigh"), Some("high"));
        assert_eq!(openai_effort("max"), Some("high"));
        assert_eq!(openai_effort("bogus"), None);
    }

    #[test]
    fn anthropic_cost_estimate_is_sane() {
        // 100k in + 10k out on Haiku ($1/$5): 0.1 + 0.05 = $0.15.
        let c = estimate_anthropic_cost("claude-haiku-4-5", 100_000, 10_000, 0, 0, 0);
        assert!((c - 0.15).abs() < 1e-9, "got {c}");
        // Web searches add $0.01 each.
        let c2 = estimate_anthropic_cost("claude-haiku-4-5", 0, 0, 0, 0, 3);
        assert!((c2 - 0.03).abs() < 1e-9, "got {c2}");
        // Opus tier costs more than Haiku for the same tokens.
        let opus = estimate_anthropic_cost("claude-opus-4-8", 100_000, 10_000, 0, 0, 0);
        assert!(opus > c);
    }

    #[test]
    fn strip_fences_and_extract_json() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("noise {\"a\":1} trailing"), "{\"a\":1}");
    }

    #[test]
    fn sse_line_buf_survives_split_utf8() {
        // A multi-byte char (é = 0xC3 0xA9) split across two network chunks
        // must reassemble instead of dropping the chunk.
        let payload = "data: {\"t\":\"caf\u{e9} — done\"}\n".as_bytes();
        let split = payload.iter().position(|&b| b == 0xC3).unwrap() + 1; // mid-codepoint
        let mut buf = SseLineBuf::new();
        buf.push(&payload[..split]);
        assert_eq!(buf.next_line(), None, "no full line yet");
        buf.push(&payload[split..]);
        assert_eq!(
            buf.next_line().as_deref(),
            Some("data: {\"t\":\"caf\u{e9} — done\"}")
        );
        assert_eq!(buf.next_line(), None);
    }

    #[test]
    fn sse_line_buf_yields_multiple_lines_per_chunk() {
        let mut buf = SseLineBuf::new();
        buf.push(b"event: x\ndata: 1\n\ndata: [DONE]\n");
        assert_eq!(buf.next_line().as_deref(), Some("event: x"));
        assert_eq!(buf.next_line().as_deref(), Some("data: 1"));
        assert_eq!(buf.next_line().as_deref(), Some(""));
        assert_eq!(buf.next_line().as_deref(), Some("data: [DONE]"));
        assert_eq!(buf.next_line(), None);
    }
}
