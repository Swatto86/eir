//! Completion entry points for [`super::client::AiClient`] (text, web search, multimodal).

use crate::ai::claude_cli;
use crate::ai::codex_cli;
use crate::ai::cursor_cli;
use crate::ai::opencode_cli;
use crate::models::CallUsage;
use anyhow::{Context, Result};
use base64::Engine;

use super::client::{AiClient, AiClientConfig};

/// A base64-encoded image attachment for multimodal completion.
/// Codex and OpenCode take files in the scratch workspace; Cursor is text-only.
#[derive(Clone, Debug)]
pub struct ImageInput {
    pub base64: String,
    /// e.g. `image/jpeg` or `image/png`.
    pub media_type: String,
}

impl ImageInput {
    fn file_name(&self, index: usize) -> String {
        let ext = match self.media_type.as_str() {
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "jpg",
        };
        format!("eir-image-{index}.{ext}")
    }
}

fn image_files(images: &[ImageInput]) -> Result<Vec<(String, Vec<u8>)>> {
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image.base64.as_bytes())
                .context("Decode attached image")?;
            Ok((image.file_name(index), bytes))
        })
        .collect()
}

impl AiClient {
    /// Completion with live web search where the CLI supports it (Codex `--search`,
    /// OpenCode `--auto`). Claude/Cursor rely on their own agent tooling.
    pub async fn complete(
        &self,
        prompt: &str,
        model_override: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let ov = model_override.trim();
        match &self.config {
            AiClientConfig::OpenCode {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                opencode_cli::call_opencode_cli(
                    configured_binary.as_deref(),
                    m,
                    "",
                    user_profile.as_deref(),
                    prompt,
                    true,
                    &[],
                )
                .await
            }
            AiClientConfig::Claude {
                configured_binary,
                user_profile,
                ..
            } => {
                let m = claude_cli::claude_cli_model(ov);
                claude_cli::call_claude_cli(
                    configured_binary.as_deref(),
                    &m,
                    &self.effort,
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
            AiClientConfig::Codex {
                configured_binary,
                model,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                codex_cli::call_codex_cli(
                    configured_binary.as_deref(),
                    m,
                    &self.effort,
                    prompt,
                    true,
                    &[],
                )
                .await
            }
            AiClientConfig::Cursor {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                cursor_cli::call_cursor_cli(
                    configured_binary.as_deref(),
                    m,
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
        }
    }

    /// Plain text completion with no web-search flag.
    pub async fn complete_text(
        &self,
        prompt: &str,
        model_override: &str,
    ) -> Result<(String, Option<CallUsage>)> {
        let ov = model_override.trim();
        match &self.config {
            AiClientConfig::OpenCode {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                opencode_cli::call_opencode_cli(
                    configured_binary.as_deref(),
                    m,
                    "",
                    user_profile.as_deref(),
                    prompt,
                    false,
                    &[],
                )
                .await
            }
            AiClientConfig::Claude {
                configured_binary,
                user_profile,
                ..
            } => {
                let m = claude_cli::claude_cli_model(ov);
                claude_cli::call_claude_cli(
                    configured_binary.as_deref(),
                    &m,
                    "",
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
            AiClientConfig::Codex {
                configured_binary,
                model,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                codex_cli::call_codex_cli(configured_binary.as_deref(), m, "", prompt, false, &[])
                    .await
            }
            AiClientConfig::Cursor {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                cursor_cli::call_cursor_cli(
                    configured_binary.as_deref(),
                    m,
                    user_profile.as_deref(),
                    prompt,
                )
                .await
            }
        }
    }

    /// Codex and OpenCode accept image files; Claude and Cursor are text-only
    /// (Cursor ask mode has no image-attach flag — documented here for callers).
    pub fn supports_images(&self) -> bool {
        matches!(
            self.config,
            AiClientConfig::Codex { .. } | AiClientConfig::OpenCode { .. }
        )
    }

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
            AiClientConfig::Codex {
                configured_binary,
                model,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                {
                    let files = image_files(images)?;
                    codex_cli::call_codex_cli(
                        configured_binary.as_deref(),
                        m,
                        "",
                        prompt,
                        false,
                        &files,
                    )
                    .await
                }
            }
            AiClientConfig::OpenCode {
                configured_binary,
                model,
                user_profile,
            } => {
                let m = if ov.is_empty() { model.as_str() } else { ov };
                let files = image_files(images)?;
                opencode_cli::call_opencode_cli(
                    configured_binary.as_deref(),
                    m,
                    "",
                    user_profile.as_deref(),
                    prompt,
                    false,
                    &files,
                )
                .await
            }
            _ => self.complete_text(prompt, model_override).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_files_decode_to_generated_names() {
        let images = [
            ImageInput {
                base64: "aGk=".to_string(),
                media_type: "image/jpeg".to_string(),
            },
            ImageInput {
                base64: "aGk=".to_string(),
                media_type: "image/png".to_string(),
            },
        ];
        let files = image_files(&images).unwrap();
        assert_eq!(files[0].0, "eir-image-0.jpg");
        assert_eq!(files[1].0, "eir-image-1.png");
        assert_eq!(files[0].1, b"hi");
        assert!(image_files(&[ImageInput {
            base64: "not base64!!".to_string(),
            media_type: "image/png".to_string(),
        }])
        .is_err());
    }
}
