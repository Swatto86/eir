use crate::config::{ApiConfig, ApiProvider};
use crate::models::{CallUsage, ClaudeDecision, PastDecision, SignalSnapshot};
#[cfg(windows)]
use crate::session::active_user_session_id;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(windows)]
use std::{ffi::OsStr, io::Read, mem::size_of, os::windows::ffi::OsStrExt};
use tracing::{debug, info, warn};
#[cfg(windows)]
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Security::{
            CreateWellKnownSid, EqualSid, GetTokenInformation, ImpersonateLoggedOnUser,
            RevertToSelf, TokenUser, WinLocalSystemSid, PSID, SECURITY_MAX_SID_SIZE, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            RemoteDesktop::WTSQueryUserToken,
            Threading::{
                CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
                ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
                CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
        UI::Shell::GetUserProfileDirectoryW,
    },
};

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

/// A base64-encoded image attachment for a multimodal completion. Only the HTTP
/// providers (Anthropic API, OpenRouter vision models) consume these; the CLI providers
/// ignore them (see `AiClient::supports_images`).
#[derive(Clone, Debug)]
pub struct ImageInput {
    pub base64: String,
    /// e.g. `image/jpeg` or `image/png`.
    pub media_type: String,
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
        configured_binary: Option<String>,
        model: String,
        user_profile: Option<String>,
    },
    /// Codex via the local CLI — no API key; borrows the user's logged-in
    /// ChatGPT subscription session. The configured path stays optional until
    /// call time because a LocalSystem service resolves and launches Codex in
    /// the active desktop user's security context.
    CodexCli {
        configured_binary: Option<String>,
        model: String,
    },
    OpenRouter {
        api_key: String,
        model: String,
    },
    /// Kilo Code via the local `kilo` CLI — borrows the machine's logged-in
    /// Kilo session (Kilo Pass / Token-Plan addons / BYOK all flow through
    /// it transparently); no API key to paste.
    KiloCli {
        configured_binary: Option<String>,
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
                let user_profile = resolve_cli_user_profile(
                    cfg.user_profile.as_deref(),
                    running_as_local_system(),
                    resolve_user_profile,
                );
                info!(
                    configured_binary = cfg.claude_cli_path.as_deref().unwrap_or("<auto>"),
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "claude_cli provider configured"
                );
                AiClientConfig::ClaudeCli {
                    configured_binary: cfg.claude_cli_path.clone().filter(|path| is_real(path)),
                    model: cfg.model.clone(),
                    user_profile,
                }
            }
            ApiProvider::CodexCli => {
                info!(
                    configured_binary = cfg.codex_cli_path.as_deref().unwrap_or("<auto>"),
                    "codex_cli provider configured"
                );
                AiClientConfig::CodexCli {
                    configured_binary: cfg.codex_cli_path.clone().filter(|path| is_real(path)),
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
            ApiProvider::KiloCli => {
                // No API key — the kilo CLI carries the user's logged-in Kilo
                // session (Kilo Pass / Token-Plan addons / BYOK all flow
                // through it transparently). Profile and binary are
                // auto-detected when not configured.
                let user_profile = resolve_cli_user_profile(
                    cfg.kilo_cli_user_profile.as_deref(),
                    running_as_local_system(),
                    resolve_kilo_cli_profile,
                );
                if cfg.model.trim().is_empty() {
                    bail!("[api] a model is required for provider = \"kilo_cli\" (e.g. kilo/minimax/minimax-m3 or kilo/anthropic/claude-sonnet-5)");
                }
                info!(
                    configured_binary = cfg.kilo_cli_path.as_deref().unwrap_or("<auto>"),
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "kilo_cli provider configured"
                );
                AiClientConfig::KiloCli {
                    configured_binary: cfg.kilo_cli_path.clone().filter(|path| is_real(path)),
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
                        &[],
                    )
                    .await
                }
                AiClientConfig::ClaudeCli {
                    configured_binary,
                    model,
                    user_profile,
                } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_claude_cli(
                        configured_binary.as_deref(),
                        m,
                        effort,
                        user_profile.as_deref(),
                        &prompt,
                    )
                    .await
                }
                AiClientConfig::CodexCli {
                    configured_binary,
                    model,
                } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_codex_cli(configured_binary.as_deref(), m, effort, &prompt, false)
                        .await
                }
                AiClientConfig::OpenRouter { api_key, model } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_openai_style(OPENROUTER_BASE, api_key, m, effort, &prompt, &[])
                        .await
                }
                AiClientConfig::KiloCli {
                    configured_binary,
                    model,
                    user_profile,
                } => {
                    let m = model_ov.unwrap_or(model);
                    self.call_kilo_cli(
                        configured_binary.as_deref(),
                        m,
                        effort,
                        user_profile.as_deref(),
                        &prompt,
                    )
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

        // Models occasionally wrap the JSON in prose; fall back to the first
        // {...last} object if a direct parse fails. Run the fallback over the ORIGINAL
        // `raw`, not `json_text` — if `strip_fences` picked the wrong block, the real
        // JSON only survives in `raw`.
        let mut decision: ClaudeDecision = match serde_json::from_str(json_text) {
            Ok(d) => d,
            Err(_) => {
                let extracted = extract_json_object(&raw);
                serde_json::from_str(extracted).with_context(|| {
                    // Truncate: the raw blob can be ~30 KB and would otherwise be cloned
                    // into st.error and rebroadcast on every 2 s UI poll until the next
                    // successful cycle.
                    format!(
                        "Failed to parse model response as JSON:\n{}",
                        char_preview(&raw, 2000)
                    )
                })?
            }
        };

        // Defensive clamp: the model could emit a confidence outside [0,1] (a garbled
        // "5.0" would render as "500%" in the UI). The policy gate is a `<` comparison
        // so this can't expand authority, but keep the value sane for display/logs.
        for p in &mut decision.problems {
            p.confidence = p.confidence.clamp(0.0, 1.0);
        }

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
        images: &[ImageInput],
    ) -> Result<(String, Option<CallUsage>)> {
        // For the analysis path, `system` carries the static instructions and is sent
        // as a CACHED system prompt so the large, unchanging guardrail block is billed
        // at the cheap cache-read rate after the first cycle instead of full input
        // price every time; the per-cycle context is the (uncached) user turn. Prompt
        // caching is GA, so no beta header is needed. Text-only completions (labeller,
        // digest) pass `system: None` and must NOT get the diagnosis system prompt.
        // With image attachments the user turn becomes a content array (image blocks
        // then the text); without, it stays a plain string (the analysis/text path).
        let content = if images.is_empty() {
            json!(user)
        } else {
            let mut blocks: Vec<serde_json::Value> = images
                .iter()
                .map(|img| {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": img.media_type,
                            "data": img.base64,
                        },
                    })
                })
                .collect();
            blocks.push(json!({ "type": "text", "text": user }));
            json!(blocks)
        };
        let mut body = json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "stream": true,
            "messages": [{"role": "user", "content": content}],
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
            bail!("Anthropic API {status}: {}", char_preview(&text, 2000));
        }

        let mut out = String::new();
        let mut lines = SseLineBuf::new();
        let mut stream = resp.bytes_stream();
        // Usage accumulates across the stream: input/cache tokens arrive in
        // message_start, output tokens in the final message_delta.
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cache_creation = 0u64;
        let mut cache_read = 0u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Anthropic stream read error")?;
            lines.push(&chunk);
            while let Some(line) = lines.next_line() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                // Anthropic's Messages API has no OpenAI-style `data: [DONE]` sentinel;
                // the stream ends with `message_stop` then the connection closes, which
                // terminates the outer loop. (The OpenAI-compatible path handles [DONE].)
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
        images: &[ImageInput],
    ) -> Result<(String, Option<CallUsage>)> {
        let url = format!("{base_url}/chat/completions");
        // Text-only uses the typed request; with images the user content becomes an
        // OpenAI-style parts array ({type:text} + {type:image_url, data-uri}). Both
        // serialise to a Value so `.json()` takes either.
        let body: serde_json::Value = if images.is_empty() {
            serde_json::to_value(OpenAiRequest {
                model,
                max_tokens: MAX_TOKENS,
                stream: true,
                messages: vec![ChatMessage {
                    role: "user",
                    content: prompt,
                }],
                stream_options: Some(StreamOptions {
                    include_usage: true,
                }),
                usage: Some(UsageInclude { include: true }),
                reasoning: openai_effort(effort).map(|e| Reasoning { effort: e }),
            })
            .unwrap_or_default()
        } else {
            let mut parts = vec![json!({ "type": "text", "text": prompt })];
            for img in images {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.base64) },
                }));
            }
            let mut b = json!({
                "model": model,
                "max_tokens": MAX_TOKENS,
                "stream": true,
                "messages": [{ "role": "user", "content": parts }],
                "stream_options": { "include_usage": true },
                "usage": { "include": true },
            });
            if let Some(e) = openai_effort(effort) {
                b["reasoning"] = json!({ "effort": e });
            }
            b
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
            bail!("model API {status}: {}", char_preview(&text, 2000));
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
        configured_binary: Option<&str>,
        model: &str,
        effort: &str,
        user_profile: Option<&str>,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        validate_cli_model_id(model, "Claude")?;
        let mut args = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ];
        if !model.is_empty() {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        if !effort.is_empty() {
            args.extend(["--effort".to_string(), effort.to_string()]);
        }
        static CLAUDE_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = CLAUDE_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let output = if running_as_local_system() {
            let binary = configured_binary.map(str::to_owned);
            let args = args.clone();
            let prompt = prompt.to_string();
            tokio::task::spawn_blocking(move || {
                run_cli_as_active_user(
                    UserCliSpec {
                        configured_binary: binary.as_deref(),
                        resolve_binary: resolve_claude_binary,
                        what: "claude CLI",
                        scratch_prefix: "eir-claude",
                        workspace_flag: None,
                        timeout_ms: 300_000,
                    },
                    &args,
                    &prompt,
                    seq,
                )
            })
            .await
            .context("Join Claude user-process task")??
        } else {
            let binary = resolve_claude_binary(configured_binary, user_profile);
            let mut command = cli_process(&binary);
            command
                .args(&args)
                .kill_on_drop(true)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(profile) = user_profile {
                command
                    .env("USERPROFILE", profile)
                    .env("HOME", profile)
                    .env("APPDATA", format!("{profile}\\AppData\\Roaming"))
                    .env("LOCALAPPDATA", format!("{profile}\\AppData\\Local"));
            }
            let child = command.spawn().context(
                "Failed to spawn the claude CLI — is it installed and logged in on this machine?",
            )?;
            let (status, stdout, stderr) =
                wait_capped(child, "claude CLI", Some(prompt.as_bytes().to_vec())).await?;
            CliProcessOutput {
                code: status.code().map(|code| code as u32).unwrap_or(u32::MAX),
                stdout,
                stderr,
            }
        };

        if output.code != 0 {
            let err = char_preview(output.stderr.trim(), 2000);
            if err.is_empty() {
                // Non-zero exit with NO diagnostic output: the CLI swallowed a
                // transient API/overload blip (an auth/config failure prints to
                // stderr instead). Marked transient (see is_transient_ai_error)
                // so analyze_with retries rather than latching the cycle red on a
                // one-off hiccup — the class that turned Eir red mid update run.
                bail!(
                    "claude CLI exited with code {} and no error output (transient)",
                    output.code
                );
            }
            bail!("claude CLI exited with code {}: {err}", output.code);
        }

        let stdout = output.stdout.trim().to_string();
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

    // ── Codex CLI subprocess (no API key — uses the logged-in ChatGPT session) ──

    async fn call_codex_cli(
        &self,
        configured_binary: Option<&str>,
        model: &str,
        effort: &str,
        prompt: &str,
        web_search: bool,
    ) -> Result<(String, Option<CallUsage>)> {
        static CODEX_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = CODEX_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let args = codex_cli_args(model, effort, web_search)?;
        let output = run_codex_cli(configured_binary, &args, prompt, seq).await?;

        if output.code != 0 {
            if let Some(err) = codex_event_error(&output.stdout) {
                bail!("codex CLI exited with code {}: {err}", output.code);
            }
            let err = char_preview(output.stderr.trim(), 2000);
            if err.is_empty() {
                bail!(
                    "codex CLI exited with code {} and no error output (transient)",
                    output.code
                );
            }
            bail!("codex CLI exited with code {}: {err}", output.code);
        }

        parse_codex_ndjson(&output.stdout)
    }

    // ── Kilo CLI subprocess (no API key — borrows the user's logged-in Kilo session) ──

    /// Run a prompt through the user's installed `kilo` CLI. The CLI carries
    /// the user's Kilo Pass / Token-Plan addons / BYOK transparently — we
    /// don't see or store any credentials. `--format json` emits NDJSON
    /// events; we collect the assistant text chunks and the last step's
    /// token/cost accounting.
    async fn call_kilo_cli(
        &self,
        configured_binary: Option<&str>,
        model: &str,
        effort: &str,
        user_profile: Option<&str>,
        prompt: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        validate_cli_model_id(model, "Kilo")?;
        static KILO_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = KILO_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut args = ["run", "--auto", "--format", "json", "--agent", "ask"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !model.is_empty() {
            args.extend(["-m".to_string(), model.to_string()]);
        }
        if let Some(variant) = kilo_cli_variant(effort) {
            args.extend(["--variant".to_string(), variant.to_string()]);
        }
        let output = if running_as_local_system() {
            let binary = configured_binary.map(str::to_owned);
            let args = args.clone();
            let prompt = prompt.to_string();
            tokio::task::spawn_blocking(move || {
                run_cli_as_active_user(
                    UserCliSpec {
                        configured_binary: binary.as_deref(),
                        resolve_binary: resolve_kilo_cli_binary,
                        what: "kilo CLI",
                        scratch_prefix: "eir-kilo",
                        workspace_flag: Some("--dir"),
                        timeout_ms: 300_000,
                    },
                    &args,
                    &prompt,
                    seq,
                )
            })
            .await
            .context("Join Kilo user-process task")??
        } else {
            let binary = resolve_kilo_cli_binary(configured_binary, user_profile);
            let workspace =
                std::env::temp_dir().join(format!("eir-kilo-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&workspace).context("Create Kilo scratch workspace")?;
            let mut command = cli_process(&binary);
            command
                .args(&args)
                .arg("--dir")
                .arg(&workspace)
                .current_dir(&workspace)
                .kill_on_drop(true)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(profile) = user_profile {
                command
                    .env("USERPROFILE", profile)
                    .env("HOME", profile)
                    .env("APPDATA", format!("{profile}\\AppData\\Roaming"))
                    .env("LOCALAPPDATA", format!("{profile}\\AppData\\Local"));
            }
            let child = command.spawn().context(
                "Failed to spawn the kilo CLI — is it installed (`npm install -g @kilocode/cli`) and logged in on this machine?",
            );
            let waited = match child {
                Ok(child) => wait_capped(child, "kilo CLI", Some(prompt.as_bytes().to_vec())).await,
                Err(error) => Err(error),
            };
            let _ = std::fs::remove_dir_all(&workspace);
            let (status, stdout, stderr) = waited?;
            CliProcessOutput {
                code: status.code().map(|code| code as u32).unwrap_or(u32::MAX),
                stdout,
                stderr,
            }
        };

        if output.code != 0 {
            // Exit 124 = kilo's own timeout (treat like ours); 1 = init/runtime
            // error. Surface stderr so the user can diagnose (e.g. "Not
            // authenticated — run `kilo` once interactively to log in").
            let err = char_preview(output.stderr.trim(), 2000);
            if err.is_empty() {
                // No diagnostic output → transient hiccup, not a config error; retry.
                bail!(
                    "kilo CLI exited with code {} and no error output (transient)",
                    output.code
                );
            }
            bail!("kilo CLI exited with code {}: {err}", output.code);
        }

        let (text, usage) = parse_kilo_ndjson(&output.stdout)?;
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
                configured_binary,
                user_profile,
                ..
            } => {
                // The CLI's built-in web search does the grounding; blank or
                // non-Claude override models coerce to the cheap haiku alias.
                let m = claude_cli_model(ov);
                self.call_claude_cli(
                    configured_binary.as_deref(),
                    &m,
                    &self.effort,
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
            AiClientConfig::CodexCli {
                configured_binary,
                model,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_codex_cli(configured_binary.as_deref(), m, &self.effort, prompt, true)
                    .await
            }
            AiClientConfig::KiloCli {
                configured_binary,
                model,
                user_profile,
            } => {
                // Kilo CLI does the work itself (web search, agent loop if
                // asked). For our app-update-check we use the same model the
                // main loop uses, with no reasoning-effort override.
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_kilo_cli(
                    configured_binary.as_deref(),
                    m,
                    "",
                    user_profile.as_deref(),
                    prompt,
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
                // No diagnosis system prompt — this is a plain text completion.
                self.call_anthropic(api_key, &m, "", None, prompt, &[])
                    .await
            }
            AiClientConfig::ClaudeCli {
                configured_binary,
                user_profile,
                ..
            } => {
                let m = claude_cli_model(ov);
                self.call_claude_cli(
                    configured_binary.as_deref(),
                    &m,
                    "",
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
            AiClientConfig::CodexCli {
                configured_binary,
                model,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_codex_cli(configured_binary.as_deref(), m, "", prompt, false)
                    .await
            }
            AiClientConfig::OpenRouter { api_key, model } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openai_style(OPENROUTER_BASE, api_key, m, "", prompt, &[])
                    .await
            }
            AiClientConfig::KiloCli {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_kilo_cli(
                    configured_binary.as_deref(),
                    m,
                    "",
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
        }
    }

    /// Whether this provider can accept image attachments. Only the HTTP providers do
    /// (and OpenRouter only with a vision-capable model — a non-vision model surfaces a
    /// per-call API error). The CLI providers don't, so the Ask handler warns and answers
    /// text-only for them.
    pub fn supports_images(&self) -> bool {
        matches!(
            self.config,
            AiClientConfig::Anthropic { .. } | AiClientConfig::OpenRouter { .. }
        )
    }

    /// Like [`complete_text`], but with image attachments. On a CLI provider (no image
    /// support) the images are ignored and it falls back to a text-only completion — the
    /// caller checks [`supports_images`] to warn the user.
    pub async fn complete_multimodal(
        &self,
        prompt: &str,
        images: &[ImageInput],
        model_override: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        if images.is_empty() {
            return self.complete_text(prompt, model_override).await;
        }
        let ov = model_override.trim();
        match &self.config {
            AiClientConfig::Anthropic { api_key, .. } => {
                let m = anthropic_web_model(ov);
                self.call_anthropic(api_key, &m, "", None, prompt, images)
                    .await
            }
            AiClientConfig::OpenRouter { api_key, model } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                self.call_openai_style(OPENROUTER_BASE, api_key, m, "", prompt, images)
                    .await
            }
            // CLI providers can't take images — answer text-only (the handler warns).
            _ => self.complete_text(prompt, model_override).await,
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

/// Build the fixed, non-interactive Codex argv. The model is the only
/// user-configurable argument that may pass through an npm `cmd /C` shim, so
/// keep it to the model-id character set before it reaches a shell.
fn codex_cli_args(model: &str, effort: &str, web_search: bool) -> Result<Vec<String>> {
    let model = model.trim();
    validate_cli_model_id(model, "Codex")?;

    let mut args = Vec::new();
    if web_search {
        args.push("--search".into());
    }
    args.extend(
        [
            "--ask-for-approval",
            "never",
            "exec",
            "--json",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--color",
            "never",
        ]
        .into_iter()
        .map(str::to_string),
    );
    if !model.is_empty() {
        args.extend(["-m".into(), model.into()]);
    }
    if let Some(level) = codex_cli_effort(model, effort) {
        args.extend(["-c".into(), format!("model_reasoning_effort={level}")]);
    }
    args.push("-".into());
    Ok(args)
}

fn validate_cli_model_id(model: &str, provider: &str) -> Result<()> {
    if model.len() > 256
        || (!model.is_empty()
            && !model.chars().all(|character| {
                character.is_ascii_alphanumeric() || "._:/@~,+=-".contains(character)
            }))
    {
        bail!("{provider} model id contains unsupported characters");
    }
    Ok(())
}

/// Current Codex models before GPT-5.6 top out at xhigh. Blank model means the
/// CLI chooses, so xhigh is the safe interpretation of Eir's `max`.
fn codex_cli_effort<'a>(model: &str, effort: &'a str) -> Option<&'a str> {
    match effort {
        "low" | "medium" | "high" | "xhigh" => Some(effort),
        "max" if model.starts_with("gpt-5.6") => Some("max"),
        "max" => Some("xhigh"),
        _ => None,
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

fn resolve_cli_user_profile(
    configured: Option<&str>,
    running_as_local_system: bool,
    resolver: fn(Option<&str>) -> Option<String>,
) -> Option<String> {
    if running_as_local_system {
        None
    } else {
        resolver(configured)
    }
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
/// otherwise tries the known install locations under the resolved user profile —
/// the native installer's `.local\bin\claude.exe`, then an npm install's bundled
/// exe (`npm i -g @anthropic-ai/claude-code` — the target its `claude.cmd` shim
/// dispatches to), then that shim itself (covers package-layout drift; the shim
/// is npm's stable entry point). All full paths, because the per-user install
/// dirs are not on LocalSystem's PATH. Finally falls back to bare "claude"
/// (must then be on PATH — interactive/dev use).
fn resolve_claude_binary(configured: Option<&str>, user_profile: Option<&str>) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for candidate in [
            format!("{up}\\.local\\bin\\claude.exe"),
            format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe"
            ),
            format!("{up}\\AppData\\Roaming\\npm\\claude.cmd"),
        ] {
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    "claude".into()
}

/// Resolve a native Codex executable under the logged-in user's profile. Full
/// paths are required for the LocalSystem service, whose PATH does not contain
/// per-user installs.
fn resolve_codex_binary(configured: Option<&str>, user_profile: Option<&str>) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for candidate in [
            format!("{up}\\AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe"),
            format!("{up}\\.codex\\packages\\standalone\\current\\bin\\codex.exe"),
            format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\bin\\codex.exe"
            ),
            format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\codex\\codex.exe"
            ),
            format!("{up}\\AppData\\Roaming\\npm\\codex.cmd"),
        ] {
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    "codex".into()
}

/// Windows npm shims are batch files and need cmd.exe; native installs run
/// directly. All values appended after this point are fixed or model-id gated.
fn cli_process(binary: &str) -> tokio::process::Command {
    let lower = binary.to_ascii_lowercase();
    if cfg!(windows) && (lower.ends_with(".cmd") || lower.ends_with(".bat")) {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.arg("/C").arg(binary);
        command
    } else {
        tokio::process::Command::new(binary)
    }
}

struct CliProcessOutput {
    code: u32,
    stdout: String,
    stderr: String,
}

async fn run_codex_cli(
    configured_binary: Option<&str>,
    args: &[String],
    prompt: &str,
    seq: u64,
) -> Result<CliProcessOutput> {
    if running_as_local_system() {
        let binary = configured_binary.map(str::to_owned);
        let args = args.to_vec();
        let prompt = prompt.to_owned();
        return tokio::task::spawn_blocking(move || {
            run_cli_as_active_user(
                UserCliSpec {
                    configured_binary: binary.as_deref(),
                    resolve_binary: resolve_codex_binary,
                    what: "codex CLI",
                    scratch_prefix: "eir-codex",
                    workspace_flag: None,
                    timeout_ms: 300_000,
                },
                &args,
                &prompt,
                seq,
            )
        })
        .await
        .context("Join Codex user-process task")?;
    }

    let profile = std::env::var("USERPROFILE").ok();
    let binary = resolve_codex_binary(configured_binary, profile.as_deref());
    let workspace = std::env::temp_dir().join(format!("eir-codex-{}-{seq}", std::process::id()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .context("Create Codex CLI scratch workspace")?;
    let mut command = cli_process(&binary);
    command
        .args(args)
        .current_dir(&workspace)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command.spawn().context(
        "Failed to spawn the codex CLI — is it installed and logged in (`codex login`) on this machine?",
    );
    let waited = match child {
        Ok(child) => wait_capped(child, "codex CLI", Some(prompt.as_bytes().to_vec())).await,
        Err(error) => Err(error),
    };
    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let (status, stdout, stderr) = waited?;
    Ok(CliProcessOutput {
        code: status.code().map(|code| code as u32).unwrap_or(u32::MAX),
        stdout,
        stderr,
    })
}

#[cfg(windows)]
struct WinHandle(HANDLE);

#[cfg(windows)]
impl Drop for WinHandle {
    fn drop(&mut self) {
        if self.0 != HANDLE::default() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
struct UserImpersonation;

#[cfg(windows)]
impl UserImpersonation {
    fn new(token: HANDLE) -> Result<Self> {
        unsafe {
            ImpersonateLoggedOnUser(token).context("Impersonate active desktop user")?;
        }
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for UserImpersonation {
    fn drop(&mut self) {
        if let Err(error) = unsafe { RevertToSelf() } {
            warn!(%error, "Failed to end active desktop user impersonation");
        }
    }
}

#[cfg(windows)]
pub(crate) fn running_as_local_system() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return true;
        }
        let _token = WinHandle(token);
        let mut bytes = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut bytes);
        if bytes == 0 {
            return true;
        }
        let mut info = vec![0u8; bytes as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            Some(info.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
        .is_err()
        {
            return true;
        }
        let user = &*(info.as_ptr().cast::<TOKEN_USER>());
        let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut sid_bytes = SECURITY_MAX_SID_SIZE;
        if CreateWellKnownSid(
            WinLocalSystemSid,
            PSID::default(),
            PSID(sid.as_mut_ptr().cast()),
            &mut sid_bytes,
        )
        .is_err()
        {
            return true;
        }
        EqualSid(user.User.Sid, PSID(sid.as_mut_ptr().cast())).is_ok()
    }
}

#[cfg(not(windows))]
fn running_as_local_system() -> bool {
    false
}

#[cfg(windows)]
fn user_profile_for_token(token: HANDLE) -> Result<String> {
    let mut chars = 0;
    unsafe {
        let _ = GetUserProfileDirectoryW(token, PWSTR::null(), &mut chars);
    }
    if chars == 0 {
        bail!("Could not resolve the active desktop user's profile");
    }
    let mut buffer = vec![0u16; chars as usize];
    unsafe {
        GetUserProfileDirectoryW(token, PWSTR(buffer.as_mut_ptr()), &mut chars)
            .context("Resolve active desktop user profile")?;
    }
    if buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf16(&buffer).context("Decode active desktop user profile")
}

#[cfg(windows)]
fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Quote one argv element using the Windows CommandLineToArgvW rules.
#[cfg(windows)]
fn quote_windows_arg(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || "\"&|<>^()!".contains(character))
    {
        return value.to_string();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        match character {
            '\\' => slashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
                quoted.push('"');
                slashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(slashes));
                slashes = 0;
                quoted.push(character);
            }
        }
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(windows)]
struct CliRedirections<'a> {
    profile: &'a str,
    stdin: &'a std::path::Path,
    stdout: &'a std::path::Path,
    stderr: &'a std::path::Path,
}

#[cfg(windows)]
fn user_cli_command_line(
    application: &str,
    binary: &str,
    args: &[String],
    files: CliRedirections<'_>,
) -> Result<String> {
    let paths = [
        binary.to_string(),
        files.profile.to_string(),
        files.stdin.to_string_lossy().into_owned(),
        files.stdout.to_string_lossy().into_owned(),
        files.stderr.to_string_lossy().into_owned(),
    ];
    if paths.iter().chain(args.iter()).any(|value| {
        value
            .chars()
            .any(|character| "\"%\r\n\0".contains(character))
    }) {
        bail!("CLI argument contains characters unsafe for Windows redirection");
    }
    let invocation = std::iter::once(binary)
        .chain(args.iter().map(String::as_str))
        .map(quote_windows_arg)
        .collect::<Vec<_>>()
        .join(" ");
    let inner = format!(
        "set \"HOME={}\" && set \"CODEX_HOME={}\\.codex\" && \
         {invocation} <{} >{} 2>{}",
        files.profile,
        files.profile,
        quote_windows_arg(&files.stdin.to_string_lossy()),
        quote_windows_arg(&files.stdout.to_string_lossy()),
        quote_windows_arg(&files.stderr.to_string_lossy())
    );
    Ok(format!(
        "{} /D /Q /V:OFF /S /C \"{inner}\"",
        quote_windows_arg(application)
    ))
}

#[cfg(windows)]
fn read_file_capped(path: &std::path::Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(CLI_OUTPUT_CAP as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// LocalSystem must never execute a user-writable CLI with SYSTEM authority.
/// Launch subscription CLIs with the active desktop user's primary token instead.
/// Cross-session handle inheritance is unsupported on Windows, so a hidden user-
/// scoped cmd process redirects through bounded scratch files in that user's profile.
#[cfg(windows)]
struct UserCliSpec<'a> {
    configured_binary: Option<&'a str>,
    resolve_binary: fn(Option<&str>, Option<&str>) -> String,
    what: &'a str,
    scratch_prefix: &'a str,
    workspace_flag: Option<&'a str>,
    timeout_ms: u32,
}

#[cfg(windows)]
fn run_cli_as_active_user(
    spec: UserCliSpec<'_>,
    args: &[String],
    prompt: &str,
    seq: u64,
) -> Result<CliProcessOutput> {
    let UserCliSpec {
        configured_binary,
        resolve_binary,
        what,
        scratch_prefix,
        workspace_flag,
        timeout_ms,
    } = spec;
    let session = active_user_session_id()
        .ok_or_else(|| anyhow::anyhow!("No single active desktop user is available for {what}"))?;
    let mut token = HANDLE::default();
    unsafe {
        WTSQueryUserToken(session, &mut token).context("Get active desktop user token")?;
    }
    let token_guard = WinHandle(token);
    let profile = user_profile_for_token(token)?;
    let binary = {
        let _user = UserImpersonation::new(token)?;
        let binary = resolve_binary(configured_binary, Some(&profile));
        if !std::path::Path::new(&binary).is_file() {
            bail!("{what} was not found for the active desktop user");
        }
        binary
    };

    let workspace = std::path::Path::new(&profile)
        .join("AppData\\Local\\Temp")
        .join(format!("{scratch_prefix}-{}-{seq}", std::process::id()));
    let result = (|| {
        let stdin = workspace.join("stdin.txt");
        let stdout = workspace.join("stdout.txt");
        let stderr = workspace.join("stderr.txt");
        {
            // Every operation beneath the user's writable profile must use the
            // user's token. Otherwise a reparse point could turn these SYSTEM
            // file operations into privileged overwrite/read/delete primitives.
            let _user = UserImpersonation::new(token)?;
            std::fs::create_dir_all(&workspace)
                .with_context(|| format!("Create {what} user scratch workspace"))?;
            std::fs::write(&stdin, prompt)?;
            std::fs::File::create(&stdout)?;
            std::fs::File::create(&stderr)?;
        }

        let application = format!(
            "{}\\System32\\cmd.exe",
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
        );
        let mut process_args = args.to_vec();
        if let Some(flag) = workspace_flag {
            process_args.push(flag.to_string());
            process_args.push(workspace.to_string_lossy().into_owned());
        }
        let command_line = user_cli_command_line(
            &application,
            &binary,
            &process_args,
            CliRedirections {
                profile: &profile,
                stdin: &stdin,
                stdout: &stdout,
                stderr: &stderr,
            },
        )
        .with_context(|| format!("Build {what} command line"))?;
        let application_w = wide_null(&application);
        let mut command_w = wide_null(&command_line);
        let workspace_w = wide_null(workspace.as_os_str());

        let mut environment = std::ptr::null_mut();
        unsafe {
            CreateEnvironmentBlock(&mut environment, token, false)
                .context("Create active desktop user environment")?;
        }
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let spawned = unsafe {
            CreateProcessAsUserW(
                token,
                PCWSTR(application_w.as_ptr()),
                PWSTR(command_w.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                Some(environment),
                PCWSTR(workspace_w.as_ptr()),
                &startup,
                &mut process,
            )
        };
        unsafe {
            let _ = DestroyEnvironmentBlock(environment);
        }
        spawned.with_context(|| format!("Launch {what} as the active desktop user"))?;

        let process_guard = WinHandle(process.hProcess);
        let thread_guard = WinHandle(process.hThread);
        let job = match unsafe { CreateJobObjectW(None, PCWSTR::null()) } {
            Ok(job) => job,
            Err(error) => {
                unsafe {
                    let _ = TerminateProcess(process.hProcess, 1);
                }
                return Err(error).with_context(|| format!("Create {what} process job"));
            }
        };
        let _job_guard = WinHandle(job);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let job_ready = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .and_then(|()| AssignProcessToJobObject(job, process.hProcess))
        };
        if let Err(error) = job_ready {
            unsafe {
                let _ = TerminateProcess(process.hProcess, 1);
            }
            return Err(error).with_context(|| format!("Contain {what} process tree"));
        }
        if unsafe { ResumeThread(thread_guard.0) } == u32::MAX {
            unsafe {
                let _ = TerminateProcess(process.hProcess, 1);
            }
            bail!("Resume {what} process failed");
        }
        match unsafe { WaitForSingleObject(process.hProcess, timeout_ms) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                unsafe {
                    let _ = TerminateProcess(process.hProcess, 1);
                    let _ = WaitForSingleObject(process.hProcess, 5_000);
                }
                bail!("{what} timed out after {}s", timeout_ms / 1_000);
            }
            _ => bail!("Wait for {what} failed"),
        }
        let mut code = 0;
        unsafe {
            GetExitCodeProcess(process_guard.0, &mut code)
                .with_context(|| format!("Read {what} exit code"))?;
        }
        let output = {
            let _user = UserImpersonation::new(token)?;
            CliProcessOutput {
                code,
                stdout: read_file_capped(&stdout)?,
                stderr: read_file_capped(&stderr)?,
            }
        };
        Ok(output)
    })();
    if let Ok(_user) = UserImpersonation::new(token) {
        let _ = std::fs::remove_dir_all(&workspace);
    }
    drop(token_guard);
    result
}

#[cfg(windows)]
fn configured_binary(configured: Option<&str>, _profile: Option<&str>) -> String {
    configured.unwrap_or_default().to_string()
}

/// Run the trusted machine Winget binary with the active desktop user's primary
/// token so LocalSystem sees the same installed-package catalog as the user.
#[cfg(windows)]
pub(crate) async fn run_winget_as_active_user(
    program: &std::path::Path,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<(i32, String)> {
    static USER_PROGRAM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let program = program.to_string_lossy().into_owned();
    let args = args.to_vec();
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let seq = USER_PROGRAM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let output = tokio::task::spawn_blocking(move || {
        run_cli_as_active_user(
            UserCliSpec {
                configured_binary: Some(&program),
                resolve_binary: configured_binary,
                what: "winget",
                scratch_prefix: "eir-winget",
                workspace_flag: None,
                timeout_ms,
            },
            &args,
            "",
            seq,
        )
    })
    .await
    .context("Join active-user winget task")??;
    let mut merged = output.stdout;
    if !output.stderr.trim().is_empty() {
        merged.push('\n');
        merged.push_str(output.stderr.trim());
    }
    Ok((i32::from_ne_bytes(output.code.to_ne_bytes()), merged))
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

/// Approximate Anthropic pay-as-you-go pricing (USD per million tokens) for usage
/// and advisor spend visibility — the API reports token counts but not cost.
/// Prices drift over time; treat this as an estimate.
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

/// Pull the concise failure message out of Codex's JSONL stream.
fn codex_event_error(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let event = serde_json::from_str::<Value>(line).ok()?;
        match event["type"].as_str()? {
            "turn.failed" => event["error"]["message"]
                .as_str()
                .map(|s| char_preview(s, 2000)),
            "error" => event["message"].as_str().map(|s| char_preview(s, 2000)),
            _ => None,
        }
    })
}

/// Parse `codex exec --json`: assistant text arrives on completed agent
/// messages and usage on `turn.completed`.
fn parse_codex_ndjson(stdout: &str) -> Result<(String, Option<CallUsage>)> {
    if let Some(error) = codex_event_error(stdout) {
        bail!("codex CLI: {error}");
    }

    let mut text = String::new();
    let mut usage = None;
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("item.completed") if event["item"]["type"].as_str() == Some("agent_message") => {
                if let Some(part) = event["item"]["text"].as_str().filter(|s| !s.is_empty()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
            }
            Some("turn.completed") => {
                let u = &event["usage"];
                let cached = u["cached_input_tokens"].as_u64().unwrap_or(0);
                usage = Some(CallUsage {
                    // Codex input_tokens includes the cached subset; Eir stores
                    // cache reads separately, so subtract it to avoid double-counting.
                    input_tokens: u["input_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(cached),
                    output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_creation: u["cache_write_input_tokens"].as_u64().unwrap_or(0),
                    cache_read: cached,
                    cost_usd: 0.0,
                });
            }
            _ => {}
        }
    }
    if text.trim().is_empty() {
        bail!("codex CLI returned no assistant text");
    }
    Ok((text, usage))
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
        // A CLI subprocess that exited non-zero with empty stderr (see
        // call_claude_cli / call_codex_cli / call_kilo_cli) — an ambiguous
        // hiccup, worth a retry.
        "no error output",
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
                // A write error usually means the child closed stdin early (it will then
                // exit non-zero and be caught by the status check). Log it so a partial
                // write that a child tolerates with a success exit isn't wholly silent.
                if let Err(e) = sin.write_all(&bytes).await {
                    warn!("{what}: failed to write full prompt to stdin: {e}");
                }
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
    let t = s.trim();
    // Only a fence at the very START of the (trimmed) response is a real code fence.
    // The old `s.find(open)` scanned the whole string, so a triple-backtick run inside
    // a log excerpt the model echoed back in "diagnosis"/"reasoning" was mistaken for
    // the opening fence and the real JSON was sliced away. `strip_prefix` matches only
    // at the start. Check the tagged fence (```json) before its bare form (```).
    for (open, close) in [
        ("```json", "```"),
        ("```", "```"),
        ("~~~json", "~~~"),
        ("~~~", "~~~"),
    ] {
        if let Some(after) = t.strip_prefix(open) {
            return after
                .find(close)
                .map(|e| &after[..e])
                .unwrap_or(after)
                .trim();
        }
    }
    t
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
    fn local_system_does_not_bind_a_subscription_cli_to_another_profile() {
        assert_eq!(
            resolve_cli_user_profile(Some(r"C:\Users\WrongProfile"), true, resolve_user_profile),
            None
        );
    }

    #[test]
    fn claude_binary_resolution_prefers_real_installs() {
        // Configured value wins outright; placeholder is ignored.
        assert_eq!(
            resolve_claude_binary(Some("D:\\tools\\claude.exe"), None),
            "D:\\tools\\claude.exe"
        );
        assert_eq!(
            resolve_claude_binary(Some("C:\\Users\\YourName\\claude.exe"), None),
            "claude"
        );
        // With a profile, candidates are probed in order: native installer,
        // npm bundled exe, npm shim; missing all → bare "claude".
        let root = std::env::temp_dir().join(format!("eir-claude-resolve-{}", std::process::id()));
        let up = root.to_string_lossy().into_owned();
        assert_eq!(resolve_claude_binary(None, Some(&up)), "claude");
        let npm_exe = root.join(
            "AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe",
        );
        std::fs::create_dir_all(npm_exe.parent().unwrap()).unwrap();
        std::fs::write(&npm_exe, b"x").unwrap();
        assert_eq!(
            resolve_claude_binary(None, Some(&up)),
            npm_exe.to_string_lossy()
        );
        // Native installer path outranks the npm exe.
        let native = root.join(".local\\bin\\claude.exe");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, b"x").unwrap();
        assert_eq!(
            resolve_claude_binary(None, Some(&up)),
            native.to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&root);
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
    fn codex_cli_args_are_safe_and_provider_appropriate() {
        let args = codex_cli_args("gpt-5.6-sol", "max", true).unwrap();
        assert_eq!(args.first().map(String::as_str), Some("--search"));
        assert!(args.windows(2).any(|w| w == ["-m", "gpt-5.6-sol"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=max"]));
        assert_eq!(args.last().map(String::as_str), Some("-"));

        let older = codex_cli_args("gpt-5.4", "max", false).unwrap();
        assert!(older
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=xhigh"]));
        assert!(codex_cli_args("gpt-5.4 & whoami", "high", false).is_err());
        assert!(codex_cli_args(&"a".repeat(257), "high", false).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn user_cli_command_line_quotes_metacharacters_and_rejects_expansion() {
        let args = vec!["--model".to_string(), "safe&literal".to_string()];
        let line = user_cli_command_line(
            r"C:\Windows\System32\cmd.exe",
            r"C:\Program Files\Codex\codex.exe",
            &args,
            CliRedirections {
                profile: r"C:\Users\Owner",
                stdin: std::path::Path::new(r"C:\Users\Owner\in.txt"),
                stdout: std::path::Path::new(r"C:\Users\Owner\out.txt"),
                stderr: std::path::Path::new(r"C:\Users\Owner\err.txt"),
            },
        )
        .unwrap();
        assert!(line.contains(r#""safe&literal""#));
        assert!(user_cli_command_line(
            r"C:\Windows\System32\cmd.exe",
            r"C:\Users\%USERNAME%\codex.exe",
            &[],
            CliRedirections {
                profile: r"C:\Users\Owner",
                stdin: std::path::Path::new(r"C:\in"),
                stdout: std::path::Path::new(r"C:\out"),
                stderr: std::path::Path::new(r"C:\err"),
            },
        )
        .is_err());
    }

    #[test]
    fn codex_ndjson_parses_text_usage_and_errors() {
        let stream = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"id\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"hello\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,",
            "\"cached_input_tokens\":40,\"cache_write_input_tokens\":3,\"output_tokens\":12}}\n"
        );
        let (text, usage) = parse_codex_ndjson(stream).unwrap();
        assert_eq!(text, "hello");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 60);
        assert_eq!(usage.cache_read, 40);
        assert_eq!(usage.cache_creation, 3);
        assert_eq!(usage.output_tokens, 12);

        let failed = r#"{"type":"turn.failed","error":{"message":"login required"}}"#;
        assert!(parse_codex_ndjson(failed)
            .unwrap_err()
            .to_string()
            .contains("login required"));
    }

    #[test]
    fn codex_binary_resolution_prefers_real_installs() {
        assert_eq!(
            resolve_codex_binary(Some("D:\\tools\\codex.exe"), None),
            "D:\\tools\\codex.exe"
        );

        let root = std::env::temp_dir().join(format!("eir-codex-resolve-{}", std::process::id()));
        let profile = root.to_string_lossy().into_owned();
        assert_eq!(resolve_codex_binary(None, Some(&profile)), "codex");

        let standalone = root.join(".codex\\packages\\standalone\\current\\bin\\codex.exe");
        std::fs::create_dir_all(standalone.parent().unwrap()).unwrap();
        std::fs::write(&standalone, b"x").unwrap();
        assert_eq!(
            resolve_codex_binary(None, Some(&profile)),
            standalone.to_string_lossy()
        );

        let app = root.join("AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&app, b"x").unwrap();
        assert_eq!(
            resolve_codex_binary(None, Some(&profile)),
            app.to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(root);
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
        // A CLI subprocess exit with empty stderr is an ambiguous hiccup — retry.
        assert!(is_transient_ai_error(&anyhow!(
            "claude CLI exited with exit code: 1 and no error output (transient)"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "kilo CLI exited with exit code: 1 and no error output (transient)"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "codex CLI exited with exit code: 1 and no error output (transient)"
        )));
        // But a CLI exit that DID print a diagnostic (auth/config) is permanent.
        assert!(!is_transient_ai_error(&anyhow!(
            "claude CLI exited with exit code: 1: Invalid API key"
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
    fn strip_fences_ignores_backticks_inside_the_payload() {
        // A model that quotes a log excerpt containing a triple-backtick block inside its
        // JSON must NOT have that inner fence mistaken for the opening one. Only a fence at
        // the trimmed start counts; here there is none, so the whole string is returned and
        // the JSON survives for `serde_json::from_str`.
        let raw = "{\"diagnosis\":\"saw ```\\nboom\\n``` in the log\",\"confidence\":0.9}";
        assert_eq!(strip_fences(raw), raw);
        // And the raw-based fallback recovers the object even if a real fence wraps prose.
        let wrapped = "Here is the answer:\n```\n{\"diagnosis\":\"x\"}\n```";
        assert_eq!(extract_json_object(wrapped), "{\"diagnosis\":\"x\"}");
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
