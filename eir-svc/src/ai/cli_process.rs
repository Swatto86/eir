//! Shared CLI subprocess helpers (caps, profile resolution, transient errors).

use anyhow::{bail, Context, Result};
use tracing::warn;

/// Per-stream memory cap for a CLI subprocess. Bounds RAM against a looping/
/// misbehaving CLI that emits huge output within the time budget.
pub(crate) const CLI_OUTPUT_CAP: usize = 16 * 1024 * 1024;

/// Windows npm shims are batch files and need cmd.exe; native installs run
/// directly. All values appended after this point are fixed or model-id gated.
pub(crate) fn cli_process(binary: &str) -> tokio::process::Command {
    let lower = binary.to_ascii_lowercase();
    if cfg!(windows) && (lower.ends_with(".cmd") || lower.ends_with(".bat")) {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.arg("/C").arg(binary);
        command
    } else {
        tokio::process::Command::new(binary)
    }
}

pub(crate) struct CliProcessOutput {
    pub code: u32,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) fn validate_cli_model_id(model: &str, provider: &str) -> Result<()> {
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

/// First `max_chars` characters of `s`. Slices by char, never by byte, so a
/// multi-byte UTF-8 codepoint straddling the limit can't panic (`&s[..n]` would).
pub(crate) fn char_preview(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// A configured value counts as "set" if it is non-empty and not the shipped
/// placeholder (the example config uses "YourName").
pub(crate) fn is_real(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty() && !v.contains("YourName")
}

/// Whether an AI-call error looks transient (worth a retry) vs. permanent (bad key,
/// 400, unknown model). Matches on the rendered error since the providers surface
/// their status in the message text.
pub(crate) fn is_transient_ai_error(e: &anyhow::Error) -> bool {
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
        // call_claude_cli / call_codex_cli / call_opencode_cli / call_cursor_cli)
        // — an ambiguous hiccup, worth a retry.
        "no error output",
    ];
    MARKERS.iter().any(|m| s.contains(m))
}

/// Read an optional child stream to at most `cap` bytes, then drain-and-discard the
/// rest (so the child never blocks on a full pipe while memory stays bounded).
pub(crate) async fn read_stream_capped<R: tokio::io::AsyncRead + Unpin>(
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
            }
        }
    }
    out
}

/// Wait for a spawned CLI child with a 300s timeout and a per-stream memory cap.
/// stdout and stderr are read concurrently (a sequential read could deadlock if the
/// child fills the other pipe's buffer). On timeout the child is killed.
pub(crate) async fn wait_capped(
    mut child: tokio::process::Child,
    what: &str,
    stdin_payload: Option<Vec<u8>>,
) -> Result<(std::process::ExitStatus, String, String)> {
    let so = child.stdout.take();
    let se = child.stderr.take();
    let sin = child.stdin.take();
    // Feed stdin CONCURRENTLY with draining stdout/stderr. Writing the whole prompt
    // first deadlocks when the prompt exceeds the OS pipe buffer and the child emits
    // output before consuming all of stdin.
    let write_fut = async move {
        if let Some(mut sin) = sin {
            if let Some(bytes) = stdin_payload {
                use tokio::io::AsyncWriteExt as _;
                if let Err(e) = sin.write_all(&bytes).await {
                    warn!("{what}: failed to write full prompt to stdin: {e}");
                }
            }
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

pub(crate) async fn resolve_cli_user_profile(
    configured: Option<String>,
    running_as_local_system: bool,
    resolver: fn(Option<&str>) -> Option<String>,
) -> Option<String> {
    if running_as_local_system {
        None
    } else {
        tokio::task::spawn_blocking(move || resolver(configured.as_deref()))
            .await
            .ok()
            .flatten()
    }
}

pub(crate) fn current_user_profile() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(Into::into)
}

pub(crate) fn resolve_profile_with_marker(
    configured: Option<&str>,
    current: Option<std::path::PathBuf>,
    marker: &[&str],
) -> Option<String> {
    if let Some(profile) = configured.filter(|profile| is_real(profile)) {
        return Some(profile.trim().to_string());
    }
    let profile = current?;
    marker
        .iter()
        .fold(profile.clone(), |path, component| path.join(component))
        .is_file()
        .then(|| profile.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_system_does_not_bind_a_subscription_cli_to_another_profile() {
        assert_eq!(
            resolve_cli_user_profile(Some(r"C:\Users\WrongProfile".to_string()), true, |_| Some(
                r"C:\Users\WrongProfile".to_string()
            ),)
            .await,
            None
        );
    }

    #[test]
    fn automatic_cli_profile_is_scoped_to_the_process_user() {
        let root = std::env::temp_dir().join(format!("eir-cli-profile-{}", std::process::id()));
        let current = root.join("current");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(current.join(".claude")).unwrap();
        std::fs::write(current.join(".claude\\.credentials.json"), b"{}").unwrap();
        assert_eq!(
            resolve_profile_with_marker(
                None,
                Some(current.clone()),
                &[".claude", ".credentials.json"]
            ),
            Some(current.to_string_lossy().into_owned())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transient_error_classification() {
        use anyhow::anyhow;
        assert!(is_transient_ai_error(&anyhow!(
            "provider API 429: rate limited"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "model API 503: unavailable"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "provider API 529: overloaded"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "claude CLI timed out after 300s"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "claude CLI exited with exit code: 1 and no error output (transient)"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "opencode CLI exited with exit code: 1 and no error output (transient)"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "cursor CLI exited with exit code: 1 and no error output (transient)"
        )));
        assert!(is_transient_ai_error(&anyhow!(
            "codex CLI exited with exit code: 1 and no error output (transient)"
        )));
        assert!(!is_transient_ai_error(&anyhow!(
            "claude CLI exited with exit code: 1: Invalid API key"
        )));
        assert!(!is_transient_ai_error(&anyhow!(
            "provider API 401: invalid key"
        )));
        assert!(!is_transient_ai_error(&anyhow!(
            "Failed to parse model response as JSON"
        )));
    }

    #[test]
    fn char_preview_never_splits_a_codepoint() {
        let s = "—".repeat(600);
        let p = char_preview(&s, 500);
        assert_eq!(p.chars().count(), 500);
        assert_eq!(char_preview("hi", 500), "hi");
    }
}
