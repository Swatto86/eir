//! Four CLI-only AI providers: OpenCode, Claude, Codex, Cursor.

use crate::ai::claude_cli;
use crate::ai::cli_ndjson::provider_cost;
use crate::ai::cli_process::{
    char_preview, is_real, is_transient_ai_error, resolve_cli_user_profile,
};
use crate::ai::cli_user::running_as_local_system;
use crate::ai::codex_cli;
use crate::ai::cursor_cli;
use crate::ai::opencode_cli;
use crate::config::{ApiConfig, ApiProvider};
use crate::models::{CallUsage, ClaudeDecision, PastDecision, SignalSnapshot};
use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

pub use crate::ai::client_complete::ImageInput;

/// Max retries for a transient AI failure before giving up on this cycle.
const MAX_AI_RETRIES: u32 = 2;

fn merge_usage(a: Option<CallUsage>, b: Option<CallUsage>) -> Option<CallUsage> {
    match (a, b) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(CallUsage {
            input_tokens: a.input_tokens.saturating_add(b.input_tokens),
            output_tokens: a.output_tokens.saturating_add(b.output_tokens),
            cache_creation: a.cache_creation.saturating_add(b.cache_creation),
            cache_read: a.cache_read.saturating_add(b.cache_read),
            cost_usd: provider_cost(a.cost_usd + b.cost_usd),
        }),
    }
}

pub struct AiClient {
    pub(crate) config: AiClientConfig,
    pub(crate) effort: String,
}

pub(crate) enum AiClientConfig {
    OpenCode {
        configured_binary: Option<String>,
        model: String,
        user_profile: Option<String>,
    },
    Claude {
        configured_binary: Option<String>,
        model: String,
        user_profile: Option<String>,
    },
    Codex {
        configured_binary: Option<String>,
        model: String,
    },
    Cursor {
        configured_binary: Option<String>,
        model: String,
        user_profile: Option<String>,
    },
}

impl AiClient {
    pub async fn new(cfg: &ApiConfig) -> Result<Self> {
        let inner = match cfg.provider {
            ApiProvider::OpenCode => {
                let user_profile = resolve_cli_user_profile(
                    cfg.opencode_cli_user_profile.clone(),
                    running_as_local_system(),
                    opencode_cli::resolve_opencode_profile,
                )
                .await;
                if cfg.model.trim().is_empty() {
                    bail!(
                        "[api] a model is required for provider = \"opencode_cli\" \
                         (e.g. ollama/llama3.2 or anthropic/claude-sonnet-4-6)"
                    );
                }
                info!(
                    configured_binary = cfg.opencode_cli_path.as_deref().unwrap_or("<auto>"),
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "opencode_cli provider configured"
                );
                AiClientConfig::OpenCode {
                    configured_binary: cfg.opencode_cli_path.clone().filter(|p| is_real(p)),
                    model: cfg.model.clone(),
                    user_profile,
                }
            }
            ApiProvider::Claude => {
                let user_profile = resolve_cli_user_profile(
                    cfg.user_profile.clone(),
                    running_as_local_system(),
                    claude_cli::resolve_claude_profile,
                )
                .await;
                info!(
                    configured_binary = cfg.claude_cli_path.as_deref().unwrap_or("<auto>"),
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "claude_cli provider configured"
                );
                AiClientConfig::Claude {
                    configured_binary: cfg.claude_cli_path.clone().filter(|p| is_real(p)),
                    model: cfg.model.clone(),
                    user_profile,
                }
            }
            ApiProvider::Codex => {
                info!(
                    configured_binary = cfg.codex_cli_path.as_deref().unwrap_or("<auto>"),
                    "codex_cli provider configured"
                );
                AiClientConfig::Codex {
                    configured_binary: cfg.codex_cli_path.clone().filter(|p| is_real(p)),
                    model: cfg.model.clone(),
                }
            }
            ApiProvider::Cursor => {
                let user_profile = resolve_cli_user_profile(
                    cfg.cursor_cli_user_profile.clone(),
                    running_as_local_system(),
                    cursor_cli::resolve_cursor_profile,
                )
                .await;
                info!(
                    configured_binary = cfg.cursor_cli_path.as_deref().unwrap_or("<auto>"),
                    user_profile = user_profile.as_deref().unwrap_or("<not found>"),
                    "cursor_cli provider configured"
                );
                AiClientConfig::Cursor {
                    configured_binary: cfg.cursor_cli_path.clone().filter(|p| is_real(p)),
                    model: cfg.model.clone(),
                    user_profile,
                }
            }
        };
        Ok(Self {
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
        let context =
            crate::ai::prompt::build_context(snapshot, history, feedback_summary, learned);
        let (raw, mut usage) = self
            .invoke_analysis_retried(&context, model_ov, effort)
            .await?;
        debug!(text = %char_preview(&raw, 500), "Raw model response");

        let mut decision: ClaudeDecision = match crate::ai::json::parse_decision(&raw) {
            Ok(d) => d,
            Err(first) => {
                warn!(
                    error = %first,
                    "Model response was not valid JSON; retrying once with a repair hint"
                );
                let repaired = format!("{context}\n\n{}", crate::ai::prompt::JSON_REPAIR_HINT);
                match self
                    .invoke_analysis_retried(&repaired, model_ov, effort)
                    .await
                {
                    Ok((raw2, usage2)) => {
                        usage = merge_usage(usage, usage2);
                        crate::ai::json::parse_decision(&raw2).with_context(|| {
                            format!(
                                "Failed to parse model response as JSON:\n{}",
                                char_preview(&raw2, 2000)
                            )
                        })?
                    }
                    Err(_) => {
                        return Err(anyhow::Error::from(first).context(format!(
                            "Failed to parse model response as JSON:\n{}",
                            char_preview(&raw, 2000)
                        )));
                    }
                }
            }
        };

        let reported_problems = decision.problems.len();
        decision.bound_model_output();
        if decision.problems.len() < reported_problems.min(5) {
            warn!(
                reported_problems,
                accepted_problems = decision.problems.len(),
                "Dropped invalid or oversized model problems"
            );
        } else if reported_problems > 5 {
            warn!(
                reported_problems,
                "Model returned more than five problems; ignored the remainder"
            );
        }

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

    async fn invoke_analysis(
        &self,
        context: &str,
        model_ov: Option<&str>,
        effort: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let blob = format!("{}\n\n{}", crate::ai::prompt::system_prompt(), context);
        match &self.config {
            AiClientConfig::OpenCode {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = model_ov.unwrap_or(model);
                opencode_cli::call_opencode_cli(
                    configured_binary.as_deref(),
                    m,
                    effort,
                    user_profile.as_deref(),
                    &blob,
                    false,
                    &[],
                )
                .await
            }
            AiClientConfig::Claude {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = model_ov.unwrap_or(model);
                claude_cli::call_claude_cli(
                    configured_binary.as_deref(),
                    m,
                    effort,
                    user_profile.as_deref(),
                    &blob,
                )
                .await
            }
            AiClientConfig::Codex {
                configured_binary,
                model,
            } => {
                let m = model_ov.unwrap_or(model);
                codex_cli::call_codex_cli(
                    configured_binary.as_deref(),
                    m,
                    effort,
                    &blob,
                    false,
                    &[],
                )
                .await
            }
            AiClientConfig::Cursor {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = model_ov.unwrap_or(model);
                cursor_cli::call_cursor_cli(
                    configured_binary.as_deref(),
                    m,
                    user_profile.as_deref(),
                    &blob,
                )
                .await
            }
        }
    }

    async fn invoke_analysis_retried(
        &self,
        context: &str,
        model_ov: Option<&str>,
        effort: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let mut attempt: u32 = 0;
        loop {
            match self.invoke_analysis(context, model_ov, effort).await {
                Ok(v) => return Ok(v),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_cost_zeros_invalid() {
        assert_eq!(provider_cost(-1.0), 0.0);
        assert_eq!(provider_cost(f64::NAN), 0.0);
        assert_eq!(provider_cost(1.25), 1.25);
    }
}
