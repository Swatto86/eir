use crate::config::{ApiConfig, ApiProvider};
use crate::models::{CallUsage, ClaudeDecision, PastDecision, SignalSnapshot};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageInclude>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct UsageInclude {
    include: bool,
}

#[derive(Deserialize)]
struct OpenAiChunk {
    choices: Option<Vec<OpenAiChoice>>,
    /// OpenRouter/free models may stream an error object instead of content.
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

// ── Claude CLI JSON envelope (claude --print --output-format json) ────────────

#[derive(Deserialize)]
struct ClaudeCliResult {
    result: Option<String>,
    total_cost_usd: Option<f64>,
    usage: Option<ClaudeCliUsage>,
}

#[derive(Deserialize)]
struct ClaudeCliUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
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
    OpenRouter {
        api_key: String,
        model: String,
    },
    ClaudeCli {
        binary: String,
        model: String,
        user_profile: Option<String>,
    },
}

impl AiClient {
    pub fn new(cfg: &ApiConfig) -> Result<Self> {
        let inner = match cfg.provider {
            ApiProvider::Anthropic => {
                let key = cfg
                    .anthropic_api_key
                    .clone()
                    .context("[api] anthropic_api_key is required for provider = \"anthropic\"")?;
                if cfg.model.trim().is_empty() {
                    bail!("[api] a model is required for provider = \"anthropic\" (e.g. claude-opus-4-8)");
                }
                AiClientConfig::Anthropic {
                    api_key: key,
                    model: cfg.model.clone(),
                }
            }
            ApiProvider::OpenAiCompatible => {
                let base_url = cfg
                    .base_url
                    .clone()
                    .context("[api] base_url is required for provider = \"openai_compatible\"")?;
                let api_key = cfg.api_key.clone().unwrap_or_else(|| "not-needed".into());
                AiClientConfig::OpenAiCompatible {
                    base_url: base_url.trim_end_matches('/').to_string(),
                    api_key,
                    model: cfg.model.clone(),
                }
            }
            ApiProvider::OpenRouter => {
                let api_key = cfg.openrouter_api_key.clone().context(
                    "[api] openrouter_api_key is required for provider = \"openrouter\"",
                )?;
                if cfg.model.trim().is_empty() {
                    bail!("[api] a model is required for provider = \"openrouter\" — pick one in Settings (e.g. nvidia/nemotron-3-super-120b-a12b:free)");
                }
                AiClientConfig::OpenRouter {
                    api_key,
                    model: cfg.model.clone(),
                }
            }
            ApiProvider::ClaudeCli => {
                let user_profile = resolve_user_profile(cfg.user_profile.as_deref());
                let binary =
                    resolve_claude_binary(cfg.claude_cli_path.as_deref(), user_profile.as_deref());
                info!(
                    binary = %binary,
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "claude_cli provider configured"
                );
                AiClientConfig::ClaudeCli {
                    binary,
                    model: cfg.model.clone(),
                    user_profile,
                }
            }
        };
        Ok(Self {
            // Generous timeout: free OpenRouter models can take 60s+ to respond.
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?,
            config: inner,
        })
    }

    pub async fn analyze(
        &self,
        snapshot: &SignalSnapshot,
        history: &[PastDecision],
        feedback_summary: Option<&str>,
    ) -> Result<(ClaudeDecision, Option<CallUsage>)> {
        let prompt = crate::ai::prompt::build(snapshot, history, feedback_summary);
        let (raw, usage) = match &self.config {
            AiClientConfig::Anthropic { api_key, model } => {
                (self.call_anthropic(api_key, model, &prompt).await?, None)
            }
            AiClientConfig::OpenAiCompatible {
                base_url,
                api_key,
                model,
            } => {
                self.call_openai_style(base_url, api_key, model, &prompt, false)
                    .await?
            }
            AiClientConfig::OpenRouter { api_key, model } => {
                self.call_openai_style(
                    "https://openrouter.ai/api/v1",
                    api_key,
                    model,
                    &prompt,
                    true,
                )
                .await?
            }
            AiClientConfig::ClaudeCli {
                binary,
                model,
                user_profile,
            } => {
                self.call_claude_cli(binary, model, user_profile.as_deref(), &prompt)
                    .await?
            }
        };

        let json_text = strip_fences(&raw);
        debug!(
            text = &json_text[..json_text.len().min(500)],
            "Raw model response"
        );

        // Free models occasionally wrap the JSON in prose; fall back to the
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
        let mut line_buf = String::new();
        let mut done = false;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            if done {
                break;
            }
            let chunk = chunk.context("Anthropic stream read error")?;
            line_buf.push_str(std::str::from_utf8(&chunk).unwrap_or(""));
            while let Some(pos) = line_buf.find('\n') {
                let line = line_buf.drain(..=pos).collect::<String>();
                if let Some(data) = line.trim().strip_prefix("data: ") {
                    if data == "[DONE]" {
                        done = true;
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

    // ── OpenAI-compatible (/v1/chat/completions) — proxy & OpenRouter ─────────

    async fn call_openai_style(
        &self,
        base_url: &str,
        api_key: &str,
        model: &str,
        prompt: &str,
        openrouter: bool,
    ) -> Result<(String, Option<CallUsage>)> {
        let url = format!("{base_url}/chat/completions");
        let body = OpenAiRequest {
            model,
            max_tokens: 4096,
            stream: true,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
            // Ask OpenRouter to stream a final usage chunk (tokens + cost). The
            // proxy path leaves these off to avoid unsupported-field errors.
            stream_options: openrouter.then_some(StreamOptions {
                include_usage: true,
            }),
            usage: openrouter.then_some(UsageInclude { include: true }),
        };

        let mut req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json");
        if openrouter {
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
            bail!("OpenAI-compatible API {status}: {text}");
        }

        let mut out = String::new();
        let mut usage: Option<CallUsage> = None;
        let mut line_buf = String::new();
        let mut done = false;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            if done {
                break;
            }
            let chunk = chunk.context("OpenAI-compatible stream read error")?;
            line_buf.push_str(std::str::from_utf8(&chunk).unwrap_or(""));
            while let Some(pos) = line_buf.find('\n') {
                let line = line_buf.drain(..=pos).collect::<String>();
                let line = line.trim();
                // The proxy sends ":ok" as a heartbeat — skip it
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

    // ── Claude CLI subprocess (no API key — uses the logged-in claude session) ──

    async fn call_claude_cli(
        &self,
        binary: &str,
        model: &str,
        user_profile: Option<&str>,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let mut cmd = tokio::process::Command::new(binary);
        // JSON output gives us the response text plus token/cost usage.
        cmd.args(["--print", "--output-format", "json"]);
        if !model.is_empty() {
            cmd.args(["--model", model]);
        }
        // Reap the (large) claude process if this future is dropped on timeout.
        cmd.kill_on_drop(true);

        // When the service runs as LocalSystem, set user-space env vars so the CLI
        // can locate the logged-in session stored in the user's AppData.
        if let Some(profile) = user_profile {
            let appdata = format!("{profile}\\AppData\\Roaming");
            let localappdata = format!("{profile}\\AppData\\Local");
            let homepath = profile.strip_prefix("C:").unwrap_or(profile);
            cmd.env("USERPROFILE", profile)
                .env("HOMEPATH", homepath)
                .env("HOMEDRIVE", "C:")
                .env("APPDATA", &appdata)
                .env("LOCALAPPDATA", &localappdata);
        }

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn claude CLI")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("Failed to write prompt to claude CLI stdin")?;
        }

        // The claude CLI is a ~245 MB binary with a cold Node start plus full
        // model inference and no streaming, so allow a generous window.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            child.wait_with_output(),
        )
        .await
        .context("claude CLI timed out after 300s")?
        .context("claude CLI process error")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "claude CLI exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() {
            bail!("claude CLI returned empty output");
        }

        // Parse the JSON envelope: { result, total_cost_usd, usage: {...} }.
        // Fall back to treating stdout as raw text if it isn't the expected shape.
        match serde_json::from_str::<ClaudeCliResult>(&stdout) {
            Ok(env) => {
                let text = env.result.unwrap_or_default();
                if text.trim().is_empty() {
                    bail!("claude CLI returned an empty result");
                }
                let usage = env.usage.map(|u| CallUsage {
                    input_tokens: u.input_tokens.unwrap_or(0),
                    output_tokens: u.output_tokens.unwrap_or(0),
                    cache_creation: u.cache_creation_input_tokens.unwrap_or(0),
                    cache_read: u.cache_read_input_tokens.unwrap_or(0),
                    cost_usd: env.total_cost_usd.unwrap_or(0.0),
                });
                Ok((text, usage))
            }
            Err(_) => Ok((stdout, None)),
        }
    }
}

/// A configured value counts as "set" if it is non-empty and not the shipped
/// placeholder (the example config uses "YourName").
fn is_real(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty() && !v.contains("YourName")
}

/// Resolve the Windows user profile whose logged-in `claude` session the service
/// should borrow. Uses the configured value when set, otherwise scans `C:\Users`
/// for the first profile holding a Claude CLI credentials file.
fn resolve_user_profile(configured: Option<&str>) -> Option<String> {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return Some(p.trim().to_string());
    }
    let users = std::fs::read_dir("C:\\Users").ok()?;
    for entry in users.flatten() {
        let dir = entry.path();
        if dir.join(".claude").join(".credentials.json").is_file() {
            return Some(dir.to_string_lossy().into_owned());
        }
    }
    None
}

/// Resolve the path to the `claude` binary. Uses the configured value when set,
/// otherwise looks for the standard install location under the resolved user
/// profile, and finally falls back to "claude" (must then be on PATH).
fn resolve_claude_binary(configured: Option<&str>, user_profile: Option<&str>) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        let candidate = format!("{up}\\.local\\bin\\claude.exe");
        if std::path::Path::new(&candidate).is_file() {
            return candidate;
        }
    }
    "claude".into()
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
