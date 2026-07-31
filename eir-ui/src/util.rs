//! Small UI-side helpers that aren't part of the (now service-side) update engine:
//! the USD→GBP rate for the cost display, and opening an external URL.

use std::{
    os::windows::process::CommandExt,
    process::{ExitStatus, Stdio},
    time::Duration,
};

/// CREATE_NO_WINDOW — keep the spawned powershell hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run a child while draining both pipes concurrently and retaining at most `cap`
/// bytes from each. Draining beyond the cap prevents a noisy child from blocking.
pub async fn run_command_capped(
    command: &mut tokio::process::Command,
    timeout: Duration,
    cap: usize,
) -> Option<(ExitStatus, Vec<u8>, Vec<u8>)> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let combined = async {
        let (stdout, stderr, status) = tokio::join!(
            read_stream_capped(stdout, cap),
            read_stream_capped(stderr, cap),
            child.wait()
        );
        (stdout, stderr, status)
    };
    match tokio::time::timeout(timeout, combined).await {
        Ok((stdout, stderr, Ok(status))) => Some((status, stdout, stderr)),
        Ok((_, _, Err(_))) => None,
        Err(_) => {
            let _ = child.start_kill();
            None
        }
    }
}

async fn read_stream_capped<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    cap: usize,
) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    let mut output = Vec::new();
    let Some(mut stream) = stream else {
        return output;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return output,
            Ok(read) if output.len() < cap => {
                output.extend_from_slice(&chunk[..read.min(cap - output.len())]);
            }
            Ok(_) => {}
        }
    }
}

/// Current USD→GBP rate for displaying costs in pounds, with a sensible offline
/// fallback.
#[tauri::command]
pub async fn gbp_per_usd() -> Result<f64, String> {
    let mut command = tokio::process::Command::new("powershell");
    command
        .args([
                "-NoProfile",
                "-Command",
                // Format with the invariant culture so the decimal separator is always
                // '.', otherwise a comma-decimal locale emits "0,79" which f64::parse
                // rejects and the live rate is silently never used.
                "try { ([double]((Invoke-RestMethod -Uri 'https://open.er-api.com/v6/latest/USD' -TimeoutSec 8).rates.GBP)).ToString([System.Globalization.CultureInfo]::InvariantCulture) } catch { '' }",
            ])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let rate = run_command_capped(&mut command, Duration::from_secs(12), 4096)
        .await
        .and_then(|(status, stdout, _)| status.success().then_some(stdout))
        .and_then(|stdout| String::from_utf8(stdout).ok())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|rate| *rate > 0.1 && *rate < 5.0)
        .unwrap_or(0.79);
    Ok(rate)
}

/// Open an http(s) URL in the user's default browser.
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http URL".into());
    }
    let script = format!("Start-Process '{}'", url.replace('\'', "''"));
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| {
        format!(
            "browser launcher exited with code {}",
            status.code().unwrap_or(-1)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn capped_reader_drains_without_growing_past_limit() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let write = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; 4096])
                .await
                .expect("write test stream");
        });

        let output = read_stream_capped(Some(reader), 64).await;
        write.await.expect("join writer");

        assert_eq!(output, vec![b'x'; 64]);
    }
}
