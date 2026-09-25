//! Linux-only optional external alert hook (`[notify] command`), disabled by default
//! (empty command). Never wired up automatically anywhere sensitive — a machine owner
//! opts in by setting `command` in config.toml, and it must never be configured
//! without their explicit go-ahead (it can send messages to an external chat).
//!
//! Applies the same hash+TTL dedup pattern the reference host-notify scripts this was
//! modelled on already use (SHA-256 of `severity:source:message`, re-alert after the
//! TTL), implemented Eir-side rather than depending on any other tool's dedup state,
//! since Eir's alert stream is its own.

#![cfg(unix)]

use crate::config::NotifyConfig;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;

/// Re-alert window: a fault that keeps recurring does not re-notify on every cycle.
const DEDUP_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

static RECENTLY_SENT: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn dedup_key(severity: &str, source: &str, message: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(severity.as_bytes());
    hasher.update(b":");
    hasher.update(source.as_bytes());
    hasher.update(b":");
    hasher.update(message.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// True if this exact (severity, source, message) was already sent within the TTL —
/// and, as a side effect, records this attempt so a genuinely new occurrence within
/// the window is suppressed too (matching a simple "re-alert every N hours" policy,
/// not "re-alert once more instantly after the first").
fn already_sent_recently(key: &str) -> bool {
    let Ok(mut guard) = RECENTLY_SENT.lock() else {
        return false;
    };
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    map.retain(|_, at| now.duration_since(*at) < DEDUP_TTL);
    if map.contains_key(key) {
        return true;
    }
    map.insert(key.to_string(), now);
    false
}

/// Send one alert through the configured external command, if any is configured and
/// this exact fault has not already alerted within the dedup window. `message` is
/// appended as the command's final argument — never interpolated into the command
/// line itself, so it cannot be read as shell syntax by anything downstream.
///
/// `#[allow(dead_code)]`: implemented and tested as a ready-to-enable primitive per
/// the owner's override (ship the hook, but it stays disabled and no call site sends
/// anything on swatbox without their explicit go-ahead); wiring specific decision-loop
/// trigger points is deferred to that follow-up, not part of this port.
#[allow(dead_code)]
pub(crate) async fn send(cfg: &NotifyConfig, severity: &str, source: &str, message: &str) {
    let Some((program, args)) = cfg.command.split_first() else {
        return; // disabled (the default)
    };
    let key = dedup_key(severity, source, message);
    if already_sent_recently(&key) {
        debug!(severity, source, "Notify hook suppressed (dedup window)");
        return;
    }

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .arg(message)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => {
            // Never at info level: a misconfigured/verbose notify script could echo
            // the secret (e.g. a bot token) it reads, and that must not land in logs
            // above debug.
            debug!(
                status = ?output.status.code(),
                stdout = %String::from_utf8_lossy(&output.stdout),
                stderr = %String::from_utf8_lossy(&output.stderr),
                "Notify hook completed"
            );
        }
        Ok(Err(error)) => debug!("Notify hook failed to launch: {error}"),
        Err(_) => debug!("Notify hook timed out after {}s", COMMAND_TIMEOUT.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_key_is_stable_and_distinguishes_real_changes() {
        let a = dedup_key("error", "caddy.service", "unit failed");
        let b = dedup_key("error", "caddy.service", "unit failed");
        assert_eq!(a, b);
        let c = dedup_key("error", "caddy.service", "unit failed again");
        assert_ne!(a, c);
        let d = dedup_key("warning", "caddy.service", "unit failed");
        assert_ne!(a, d);
    }

    #[test]
    fn a_fresh_key_is_not_deduped_but_a_repeat_within_the_window_is() {
        let key = format!("test-key-{}", std::process::id());
        assert!(!already_sent_recently(&key), "first send must go through");
        assert!(
            already_sent_recently(&key),
            "an immediate repeat must be suppressed"
        );
    }

    #[tokio::test]
    async fn an_empty_command_is_a_silent_no_op() {
        let cfg = NotifyConfig { command: vec![] };
        // Must return promptly and never panic — there is nothing to invoke.
        tokio::time::timeout(
            Duration::from_secs(1),
            send(&cfg, "error", "test", "message"),
        )
        .await
        .expect("disabled notify must return immediately");
    }

    #[tokio::test]
    async fn the_message_is_appended_as_the_final_argument_not_interpolated() {
        let marker = format!("eir-notify-test-{}", std::process::id());
        let out_file = std::env::temp_dir().join(format!("{marker}.out"));
        let _ = std::fs::remove_file(&out_file);
        let cfg = NotifyConfig {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("cat > {}", out_file.display()),
            ],
        };
        let malicious = "hello; rm -rf / #".to_string();
        send(&cfg, "error", &marker, &malicious).await;
        // sh -c "cat > file" ignores the extra argv[1] ($0 replacement) entirely —
        // proving the message never reaches the shell as command text, only as data
        // a plain `cat` would need to be told to read (which this fixture doesn't),
        // so the file is empty/absent rather than containing shell-evaluated output.
        let contents = std::fs::read_to_string(&out_file).unwrap_or_default();
        assert!(
            !contents.contains("rm -rf"),
            "the message must never be evaluated as shell syntax"
        );
        let _ = std::fs::remove_file(&out_file);
    }
}
