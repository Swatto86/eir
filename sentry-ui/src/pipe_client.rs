use anyhow::Result;
use sentry_proto::{ServiceMsg, StatusPayload, UiMsg, PIPE_NAME};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::ClientOptions,
    sync::mpsc,
};
use tracing::{info, warn};

pub type SharedStatus = Arc<Mutex<StatusPayload>>;

/// Runs the pipe client loop forever, reconnecting on disconnect.
/// Updates `status` whenever a StatusPayload arrives from the service.
pub async fn run(status: SharedStatus, mut cmd_rx: mpsc::Receiver<UiMsg>) {
    loop {
        match connect_and_run(&status, &mut cmd_rx).await {
            Ok(()) => {}
            Err(e) => {
                warn!("Pipe client disconnected: {e}");
                let mut s = status.lock().unwrap();
                s.status = "ServiceDisconnected".to_string();
                s.error = Some(
                    "Sentry service is not running. \
                     Run as Administrator: sentry-svc.exe install \
                     then sc start SentrySvc"
                        .to_string(),
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_run(status: &SharedStatus, cmd_rx: &mut mpsc::Receiver<UiMsg>) -> Result<()> {
    // Keep trying until the pipe is available (service may still be starting).
    let client = loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(2) => {
                // ERROR_FILE_NOT_FOUND — pipe not created yet
                return Err(anyhow::anyhow!("Service pipe not available: {e}"));
            }
            Err(e) if e.raw_os_error() == Some(231) => {
                // ERROR_PIPE_BUSY — retry
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    };

    info!("Connected to Sentry service pipe");

    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => return Ok(()), // service disconnected
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<ServiceMsg>(trimmed) {
                                Ok(ServiceMsg::Status(payload)) => {
                                    *status.lock().unwrap() = payload;
                                }
                                Err(e) => warn!("Bad service message: {e}"),
                            }
                        }
                        line.clear();
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                let mut json = serde_json::to_string(&cmd)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
            }
        }
    }
}
