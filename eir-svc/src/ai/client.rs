use crate::config::{ApiConfig, ApiProvider};
use crate::models::{CallUsage, ClaudeDecision, PastDecision, SignalSnapshot};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
/// Max retries for a transient AI failure before giving up on this cycle.
const MAX_AI_RETRIES: u32 = 2;
/// Output-token ceiling per analysis. A busy machine can produce many problems; at
/// 4096 the structured JSON was truncated mid-object and the whole cycle failed to
/// parse. Current models comfortably support this larger budget (billed on actual use).
const MAX_TOKENS: u32 = 8192;

// ── OpenAI-compatible request/response (OpenRouter) ────────────────────────────

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
    /// Reasoning effort (OpenRouter reasoning models).
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
    Anthropic {
        api_key: String,
        model: String,
    },
    /// Claude via the local `claude` CLI — no API key; borrows the machine's
    /// logged-in Claude subscription session.
    ClaudeCli {
        binary: String,
        model: String,
        user_profile: Option<String>,
    },
    OpenRouter {
        api_key: String,
        model: String,
    },
    /// Kilo Code via the local `kilo` CLI — borrows the machine's logged-in
    /// Kilo session (Kilo Pass / Token-Plan addons / BYOK all flow through
    /// it transparently); no API key to paste.
    KiloCli {
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
            ApiProvider::ClaudeCli => {
                // No key required — the CLI carries the user's Claude
                // subscription session. Profile and binary are auto-detected
                // when not configured.
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
            ApiProvider::KiloCli => {
                // No API key — the kilo CLI carries the user's logged-in Kilo
                // session (Kilo Pass / Token-Plan addons / BYOK all flow
                // through it transparently). Profile and binary are
                // auto-detected when not configured.
                let user_profile = resolve_kilo_cli_profile(cfg.kilo_cli_user_profile.as_deref());
                let binary =
                    resolve_kilo_cli_binary(cfg.kilo_cli_path.as_deref(), user_profile.as_deref());
                if cfg.model.trim().is_empty() {
                    bail!("[api] a model is required for provider = \"kilo_cli\" (e.g. kilo/minimax/minimax-m2.5 or anthropic/claude-sonnet-4.6)");
                }
                info!(
                    binary = %binary,
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "kilo_cli provider configured"
                );
                AiClientConfig::KiloCli {
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
        // The per-cycle context is the changing half; the static SYSTEM_PROMPT is
        // prepended for the single-blob providers, or sent as a cached system prompt on
        // the Anthropic path.
        let context =
            crate::ai::prompt::build_context(snapshot, history, feedback_summary, learned);
        let prompt = format!("{}\n\n{}", crate::ai::prompt::SYSTEM_PROMPT, context);
        // Bounded retry on TRANSIENT failures only. Each call returns Err before
        // yielding any text, so retrying the whole call is safe (no partial output to
        // corrupt). This runs off the decision loop, so the backoff doesn't stall the
        // UI; a permanent error (bad key, 400) is returned immediately without retry.
        let mut attempt: u32 = 0;
        let (raw, usage) = loop {
            let result = match &self.config {
                AiClientConfig::Anthropic { api_key, model } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_anthropic(
                        api_key,
                        m,
                        effort,
                        Some(crate::ai::prompt::SYSTEM_PROMPT),
                        &context,
                    )
                    .await
                }
                AiClientConfig::ClaudeCli {
                    binary,
                    model,
                    user_profile,
                } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_claude_cli(binary, m, effort, user_profile.as_deref(), &prompt)
                        .await
                }
                AiClientConfig::OpenRouter { api_key, model } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_openai_style(OPENROUTER_BASE, api_key, m, effort, &prompt)
                        .await
                }
                AiClientConfig::KiloCli {
                    binary,
                    model,
                    user_profile,
                } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_kilo_cli(binary, m, effort, user_profile.as_deref(), &prompt)
                        .await
                }
            };
            match result {
                Ok(v) => break v,
                Err(e) if attempt < MAX_AI_RETRIES && is_transient_ai_error(&e) => {
                    attempt += 1;
                    let backoff = std::time::Duration::from_secs(2u64.pow(attempt));
                    warn!(
                        "AI call failed (transient, attempt {attempt}/{MAX_AI_RETRIES}), \
                         retrying in {}s: {e}",
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        };

        let json_text = strip_fences(&raw);
        debug!(text = %char_preview(json_text, 500), "Raw model response");

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
        system: Option<&str>,
        user: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        // For the analysis path, `system` carries the static instructions and is sent
        // as a CACHED system prompt so the large, unchanging guardrail block is billed
        // at the cheap cache-read rate after the first cycle instead of full input
        // price every time; the per-cycle context is the (uncached) user turn. Prompt
        // caching is GA, so no beta header is needed. Text-only completions (labeller,
        // digest) pass `system: None` and must NOT get the diagnosis system prompt.
        let mut body = json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "stream": true,
            "messages": [{"role": "user", "content": user}],
        });
        if let Some(sys) = system {
            body["system"] = json!([{
                "type": "text",
                "text": sys,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
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
                    Some("content_block_delta")
                        if ev["delta"]["type"].as_str() == Some("text_delta") =>
                    {
                        out.push_str(ev["delta"]["text"].as_str().unwrap_or(""));
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
            usage: Some(UsageInclude { include: true }),
            reasoning: openai_effort(effort).map(|e| Reasoning { effort: e }),
        };

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            // OpenRouter app attribution (optional but recommended).
            .header("HTTP-Referer", "https://github.com/Swatto86/eir")
            .header("X-Title", "Eir")
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

    // ── Claude CLI subprocess (no API key — uses the logged-in claude session) ──

    async fn call_claude_cli(
        &self,
        binary: &str,
        model: &str,
        effort: &str,
        user_profile: Option<&str>,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let mut cmd = tokio::process::Command::new(binary);
        // JSON output gives us the response text plus token/cost usage.
        cmd.args(["--print", "--output-format", "json"]);
        if !model.is_empty() {
            cmd.args(["--model", model]);
        }
        // Reasoning effort (low|medium|high|xhigh|max); validated upstream.
        if !effort.is_empty() {
            cmd.args(["--effort", effort]);
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

        let child = cmd.spawn().context(
            "Failed to spawn the claude CLI — is it installed and logged in on this machine?",
        )?;

        // The claude CLI is a large binary with a cold Node start plus full
        // model inference and no streaming, so allow a generous window. The prompt is
        // written to stdin concurrently with draining stdout/stderr (inside
        // wait_capped) so a large prompt can't deadlock the pipes. Output is read with
        // a memory cap so a runaway CLI can't OOM the service.
        let (status, stdout_raw, stderr_raw) =
            wait_capped(child, "claude CLI", Some(prompt.as_bytes().to_vec())).await?;

        if !status.success() {
            bail!("claude CLI exited with {}: {}", status, stderr_raw.trim());
        }

        let stdout = stdout_raw.trim().to_string();
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
                // total_cost_usd is the *equivalent* API cost — the call itself
                // is covered by the subscription.
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

    // ── Kilo CLI subprocess (no API key — borrows the user's logged-in Kilo session) ──

    /// Run a prompt through the user's installed `kilo` CLI. The CLI carries
    /// the user's Kilo Pass / Token-Plan addons / BYOK transparently — we
    /// don't see or store any credentials. `--format json` emits NDJSON
    /// events; we collect the assistant text chunks and the last step's
    /// token/cost accounting.
    async fn call_kilo_cli(
        &self,
        binary: &str,
        model: &str,
        effort: &str,
        user_profile: Option<&str>,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        // Give the agent a writable workspace so it doesn't refuse to start.
        // Each call gets a fresh empty dir — the agent has nothing to operate
        // on, so it just answers the prompt. Key it per-call (pid + a monotonic
        // sequence), not just per-process, so a concurrent analysis and labeller call
        // don't point two `kilo` processes at the same workspace; cleaned up after the
        // call so it doesn't litter %TEMP%.
        static KILO_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = KILO_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let workspace =
            std::env::temp_dir().join(format!("eir-kilo-{}-{}", std::process::id(), seq));
        let _ = std::fs::create_dir_all(&workspace);

        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(["run", "--auto", "--format", "json", "--agent", "ask"]);
        if !model.is_empty() {
            cmd.args(["-m", model]);
        }
        if let Some(variant) = kilo_cli_variant(effort) {
            cmd.args(["--variant", variant]);
        }
        cmd.arg("--dir").arg(&workspace);
        // Reap the subprocess if the future is dropped on timeout.
        cmd.kill_on_drop(true);

        // Borrow the user's logged-in Kilo session when running as
        // LocalSystem. Same pattern as `call_claude_cli` — APPDATA /
        // LOCALAPPDATA are what the Node CLI uses to locate its session.
        if let Some(profile) = user_profile {
            let appdata = format!("{profile}\\AppData\\Roaming");
            let localappdata = format!("{profile}\\AppData\\Local");
            let homepath = profile.strip_prefix("C:").unwrap_or(profile);
            cmd.env("USERPROFILE", profile)
                .env("HOMEPATH", homepath)
                .env("HOMEDRIVE", "C:")
                .env("APPDATA", &appdata)
                .env("LOCALAPPDATA", &localappdata)
                // The kilo CLI is a Bun-compiled binary; Bun tooling can
                // consult HOME on Windows for some path resolution.
                .env("HOME", profile);
        }

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let child = cmd.spawn().context(
            "Failed to spawn the kilo CLI — is it installed (`npm install -g @kilocode/cli`) and logged in on this machine?",
        )?;

        // The kilo CLI is a Node binary with a cold start plus agent loop
        // (--auto) and JSON event emission, so allow a generous window. The prompt is
        // written to stdin concurrently with draining stdout/stderr (inside
        // wait_capped) so a large prompt can't deadlock the pipes; dropping stdin
        // afterwards signals EOF so kilo exits. Output is read with a memory cap so a
        // runaway agent loop can't OOM the service.
        let waited = wait_capped(child, "kilo CLI", Some(prompt.as_bytes().to_vec())).await;
        // Reclaim the scratch workspace whether the call succeeded or errored.
        let _ = std::fs::remove_dir_all(&workspace);
        let (status, stdout, stderr_raw) = waited?;

        if !status.success() {
            // Exit 124 = kilo's own timeout (treat like ours); 1 = init/runtime
            // error. Surface stderr so the user can diagnose (e.g. "Not
            // authenticated — run `kilo` once interactively to log in").
            bail!("kilo CLI exited with {}: {}", status, stderr_raw.trim());
        }

        let (text, usage) = parse_kilo_ndjson(&stdout)?;
        if text.trim().is_empty() {
            bail!("kilo CLI returned no assistant text");
        }
        Ok((text, usage))
    }
}

// ── Web-search completion (app-update plan / diagnosis) ───────────────────────
//
// A general "ask the model this prompt, with live web search where available"
// entry point, used by the updater to resolve an official installer URL and to
// read failure errors. OpenRouter uses its `web` plugin; Anthropic uses the
// native web_search server tool; the Kilo CLI does its own web search as part
// of its agent loop.

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
            AiClientConfig::ClaudeCli {
                binary,
                user_profile,
                ..
            } => {
                // The CLI's built-in web search does the grounding; blank or
                // non-Claude override models coerce to the cheap haiku alias.
                let m = claude_cli_model(ov);
                self.call_claude_cli(binary, &m, &self.effort, user_profile.as_deref(), prompt)
                    .await
            }
            AiClientConfig::KiloCli {
                binary,
                model,
                user_profile,
            } => {
                // Kilo CLI does the work itself (web search, agent loop if
                // asked). For our app-update-check we use the same model the
                // main loop uses, with no reasoning-effort override.
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_kilo_cli(binary, m, "", user_profile.as_deref(), prompt)
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
                // No diagnosis system prompt — this is a plain text completion.
                self.call_anthropic(api_key, &m, "", None, prompt).await
            }
            AiClientConfig::ClaudeCli {
                binary,
                user_profile,
                ..
            } => {
                let m = claude_cli_model(ov);
                self.call_claude_cli(binary, &m, "", user_profile.as_deref(), prompt)
                    .await
            }
            AiClientConfig::OpenRouter { api_key, model } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openai_style(OPENROUTER_BASE, api_key, m, "", prompt)
                    .await
            }
            AiClientConfig::KiloCli {
                binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_kilo_cli(binary, m, "", user_profile.as_deref(), prompt)
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

/// Map a requested model to one the Claude CLI accepts. Claude aliases
/// (`haiku`/`sonnet`/`opus`) and any `claude*` id pass through; everything else
/// — blank, or a non-Claude id such as an OpenRouter model — becomes `haiku`
/// (cheap default for the web/labelling calls on this provider).
pub(crate) fn claude_cli_model(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_lowercase();
    let is_claude =
        matches!(lower.as_str(), "haiku" | "sonnet" | "opus") || lower.starts_with("claude");
    if is_claude {
        m.to_string()
    } else {
        "haiku".to_string()
    }
}

/// Map Eir's reasoning-effort levels onto the kilo CLI's `--variant` values
/// (low|medium|high). The CLI rejects unknown variants, so we collapse
/// xhigh/max to high and ignore anything else (empty = don't pass the flag).
fn kilo_cli_variant(effort: &str) -> Option<&'static str> {
    match effort {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
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

/// Resolve the Windows user profile whose logged-in Kilo session the service
/// should borrow. Uses the configured value when set, otherwise scans
/// `C:\Users` for the first profile holding the Kilo CLI's session file at
/// `.local\share\kilo\auth.json` — a single JSON object keyed by provider
/// name (`"kilo": {"type":"oauth",...}` for the Kilo Pass/Token-Plan login,
/// plus any BYOK provider entries added via `kilo auth login`, e.g.
/// `"openrouter": {"type":"api",...}`).
fn resolve_kilo_cli_profile(configured: Option<&str>) -> Option<String> {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return Some(p.trim().to_string());
    }
    let users = std::fs::read_dir("C:\\Users").ok()?;
    for entry in users.flatten() {
        let dir = entry.path();
        if dir
            .join(".local")
            .join("share")
            .join("kilo")
            .join("auth.json")
            .is_file()
        {
            return Some(dir.to_string_lossy().into_owned());
        }
    }
    None
}

/// Resolve the path to the `kilo` binary. Uses the configured value when set,
/// otherwise tries the platform-specific executable `npm install -g
/// @kilocode/cli` places under the resolved user profile — Windows installs
/// resolve to either the `cli-windows-x64` or `-baseline` sibling depending
/// on CPU feature detection at install time, so both are tried — then the
/// npm shim (`kilo.cmd`, a per-user PATH entry LocalSystem doesn't have),
/// and finally falls back to bare `"kilo"` (PATH-reliant, for interactive/
/// non-service use).
fn resolve_kilo_cli_binary(configured: Option<&str>, user_profile: Option<&str>) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for variant in ["cli-windows-x64", "cli-windows-x64-baseline"] {
            let candidate = format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@kilocode\\cli\\node_modules\\@kilocode\\{variant}\\bin\\kilo.exe"
            );
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
        let shim = format!("{up}\\AppData\\Roaming\\npm\\kilo.cmd");
        if std::path::Path::new(&shim).is_file() {
            return shim;
        }
    }
    "kilo".into()
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

/// Parse the kilo CLI's `--format json` NDJSON output. The CLI nests each
/// event's actual payload under a `part` object (e.g.
/// `{"type":"text","part":{"type":"text","text":"..."}}`); we read fields
/// there first and fall back to the top-level shape for older/alternate
/// event variants. We concatenate `text` events into the final assistant
/// reply and keep the latest `step_finish` event's token/cost accounting.
/// Unknown event types and individual parse failures are skipped — the wire
/// format is still evolving upstream and we don't want a single bad event to
/// sink the whole response.
fn parse_kilo_ndjson(stdout: &str) -> Result<(String, Option<CallUsage>)> {
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
            // step_finish arrives at the end of each agent step; the last
            // one wins. Kilo also emits `session_end` and `step_start`, both
            // of which we deliberately ignore.
            Some("step_finish") => last_step = Some(ev),
            _ => {}
        }
    }
    let usage = last_step.map(|s| {
        // Prefer the part-nested tokens/cost the real CLI emits; fall back
        // to the top-level shape (older/alternate event variants).
        let part_tokens = &s["part"]["tokens"];
        let tokens = if part_tokens.is_null() {
            &s["tokens"]
        } else {
            part_tokens
        };
        // Kilo's wire format puts tokens under `tokens.{input,output,cache.read,cache.write}`;
        // be permissive — older betas used `tokens.{input_tokens,output_tokens}`.
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
            cost_usd: cost,
        }
    });
    Ok((text, usage))
}

/// First `max_chars` characters of `s`. Slices by char, never by byte, so a
/// multi-byte UTF-8 codepoint straddling the limit can't panic (`&s[..n]` would).
fn char_preview(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Whether an AI-call error looks transient (worth a retry) vs. permanent (bad key,
/// 400, unknown model). Matches on the rendered error since the providers surface
/// their status in the message text.
fn is_transient_ai_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    const MARKERS: &[&str] = &[
        "429",
        "500",
        "502",
        "503",
        "504",
        "529",
        "timed out",
        "timeout",
        "overloaded",
        "temporarily",
        "rate limit",
        "connection",
        "stream read error",
        "stream error",
    ];
    MARKERS.iter().any(|m| s.contains(m))
}

/// Per-stream memory cap for a CLI subprocess. Bounds RAM against a looping/
/// misbehaving CLI that emits huge output within the time budget.
const CLI_OUTPUT_CAP: usize = 16 * 1024 * 1024;

/// Read an optional child stream to at most `cap` bytes, then drain-and-discard the
/// rest (so the child never blocks on a full pipe while memory stays bounded).
async fn read_stream_capped<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    cap: usize,
) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    let Some(mut r) = stream else {
        return out;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < cap {
                    let take = n.min(cap - out.len());
                    out.extend_from_slice(&chunk[..take]);
                }
                // Beyond the cap the bytes are discarded, not buffered.
            }
        }
    }
    out
}

/// Wait for a spawned CLI child with a 300s timeout and a per-stream memory cap.
/// stdout and stderr are read concurrently (a sequential read could deadlock if the
/// child fills the other pipe's buffer). On timeout the child is killed.
async fn wait_capped(
    mut child: tokio::process::Child,
    what: &str,
    stdin_payload: Option<Vec<u8>>,
) -> Result<(std::process::ExitStatus, String, String)> {
    let so = child.stdout.take();
    let se = child.stderr.take();
    let sin = child.stdin.take();
    // Feed stdin CONCURRENTLY with draining stdout/stderr. Writing the whole prompt
    // first (as the callers used to) deadlocks when the prompt exceeds the OS pipe
    // buffer (~64 KB) and the child emits output before consuming all of stdin: the
    // child blocks on a full stdout pipe while we block on a full stdin pipe. Dropping
    // stdin after the write signals EOF so the child proceeds.
    let write_fut = async move {
        if let Some(mut sin) = sin {
            if let Some(bytes) = stdin_payload {
                use tokio::io::AsyncWriteExt as _;
                let _ = sin.write_all(&bytes).await;
            }
            // `sin` drops here → stdin closes → EOF.
        }
    };
    let combined = async {
        let (o, e, s, _w) = tokio::join!(
            read_stream_capped(so, CLI_OUTPUT_CAP),
            read_stream_capped(se, CLI_OUTPUT_CAP),
            child.wait(),
            write_fut,
        );
        (o, e, s)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(300), combined).await {
        Ok((o, e, s)) => {
            let status = s.with_context(|| format!("{what} process error"))?;
            Ok((
                status,
                String::from_utf8_lossy(&o).into_owned(),
                String::from_utf8_lossy(&e).into_owned(),
            ))
        }
        Err(_) => {
            let _ = child.start_kill();
            bail!("{what} timed out after 300s")
        }
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
    fn claude_cli_model_coercion() {
        // Aliases and claude-* ids pass through; blank/non-Claude → haiku.
        assert_eq!(claude_cli_model(""), "haiku");
        assert_eq!(claude_cli_model("haiku"), "haiku");
        assert_eq!(claude_cli_model("opus"), "opus");
        assert_eq!(claude_cli_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(claude_cli_model("openrouter/free"), "haiku");
    }

    #[test]
    fn claude_cli_envelope_parses_usage() {
        // The wire-format decode for the CLI's JSON envelope.
        let raw = r#"{"result":"{\"analysis\":\"ok\"}","total_cost_usd":0.0123,
            "usage":{"input_tokens":100,"output_tokens":20,
            "cache_creation_input_tokens":5,"cache_read_input_tokens":50}}"#;
        let env: ClaudeCliResult = serde_json::from_str(raw).unwrap();
        assert_eq!(env.result.as_deref(), Some("{\"analysis\":\"ok\"}"));
        assert_eq!(env.total_cost_usd, Some(0.0123));
        let u = env.usage.unwrap();
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.cache_read_input_tokens, Some(50));
        // Partial envelopes (no usage) still parse.
        let sparse: ClaudeCliResult = serde_json::from_str(r#"{"result":"hi"}"#).unwrap();
        assert!(sparse.usage.is_none());
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
    fn transient_error_classification() {
        use anyhow::anyhow;
        // Retryable provider/network conditions.
        assert!(is_transient_ai_error(&anyhow!(
            "Anthropic API 429: rate limited"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "model API 503: unavailable"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "Anthropic API 529: overloaded"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "claude CLI timed out after 300s"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "Anthropic stream read error"
        )));
        // Permanent conditions must NOT be retried.
        assert!(!is_transient_ai_error(&anyhow!(
            "Anthropic API 401: invalid x-api-key"
        )));
        assert!(!is_transient_ai_error(&anyhow!(
            "model API 400: unknown model"
        )));
        assert!(!is_transient_ai_error(&anyhow!(
            "Failed to parse model response as JSON"
        )));
    }

    #[test]
    fn char_preview_never_splits_a_codepoint() {
        // 600 em-dashes = 1800 bytes; byte-slicing at 500 would panic mid-codepoint.
        let s = "—".repeat(600);
        let p = char_preview(&s, 500);
        assert_eq!(p.chars().count(), 500);
        // Shorter-than-limit input is returned whole.
        assert_eq!(char_preview("hi", 500), "hi");
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

    #[test]
    fn kilo_ndjson_collects_text_and_keeps_last_step_usage() {
        // A realistic stream matching the real CLI's part-nested wire format:
        // noise events, text chunks, step_finish with tokens + cost under
        // "part", then session_end. Text must concatenate in order; usage
        // must come from the LAST step_finish.
        let stream = r#"
{"type":"step_start","sessionID":"abc","part":{"type":"step-start"}}
{"type":"text","part":{"type":"text","text":"hello "}}
{"type":"text","part":{"type":"text","text":"world"}}
{"type":"step_finish","part":{"type":"step-finish","cost":0.001,"tokens":{"input":100,"output":20,"cache":{"read":0,"write":50}}}}
{"type":"text","part":{"type":"text","text":"!"}}
{"type":"step_finish","part":{"type":"step-finish","cost":0.002,"tokens":{"input":150,"output":25,"cache":{"read":10,"write":50}}}}
{"type":"session_end"}
"#;
        let (text, usage) = parse_kilo_ndjson(stream).unwrap();
        assert_eq!(text, "hello world!");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 150);
        assert_eq!(u.output_tokens, 25);
        assert_eq!(u.cache_read, 10);
        assert_eq!(u.cache_creation, 50);
        assert!((u.cost_usd - 0.002).abs() < 1e-9);
    }

    #[test]
    fn kilo_ndjson_tolerates_garbage_lines_and_alternate_token_shape() {
        // Random log noise on stdout + an older beta that used
        // input_tokens/output_tokens instead of input/output.
        let stream = "Some log preamble\n[stderr-redir] debug: hi\n\
            {\"type\":\"text\",\"text\":\"ok\"}\n\
            {\"type\":\"step_finish\",\"cost\":0.01,\"tokens\":{\"input_tokens\":7,\"output_tokens\":3,\"cache\":{\"read\":0,\"write\":0}}}\n";
        let (text, usage) = parse_kilo_ndjson(stream).unwrap();
        assert_eq!(text, "ok");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
    }

    #[test]
    fn kilo_cli_variant_mapping() {
        assert_eq!(kilo_cli_variant(""), None);
        assert_eq!(kilo_cli_variant("low"), Some("low"));
        assert_eq!(kilo_cli_variant("medium"), Some("medium"));
        assert_eq!(kilo_cli_variant("high"), Some("high"));
        assert_eq!(kilo_cli_variant("xhigh"), Some("high"));
        assert_eq!(kilo_cli_variant("max"), Some("high"));
        assert_eq!(kilo_cli_variant("bogus"), None);
    }
}
