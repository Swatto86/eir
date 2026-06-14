use anyhow::Result;
use eir_proto::{ServiceMsg, StatusPayload, UiMsg, PIPE_NAME};
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
pub async fn run(status: SharedStatus, cmd_rx: mpsc::Receiver<UiMsg>) {
    run_on(status, cmd_rx, PIPE_NAME).await
}

async fn run_on(status: SharedStatus, mut cmd_rx: mpsc::Receiver<UiMsg>, pipe_name: &str) {
    loop {
        match connect_and_run(&status, &mut cmd_rx, pipe_name).await {
            Ok(()) => {}
            Err(e) => {
                warn!("Pipe client disconnected: {e}");
                let mut s = status.lock().unwrap();
                s.status = "ServiceDisconnected".to_string();
                s.error = Some(
                    "Eir service is not running. \
                     Run as Administrator: eir-svc.exe install \
                     then sc start EirSvc"
                        .to_string(),
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_run(
    status: &SharedStatus,
    cmd_rx: &mut mpsc::Receiver<UiMsg>,
    pipe_name: &str,
) -> Result<()> {
    // Keep trying until the pipe is available (service may still be starting).
    let client = loop {
        match ClientOptions::new().open(pipe_name) {
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

    info!("Connected to Eir service pipe");

    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);

    // Read and write run as two independent loops. The previous design polled
    // both in one `select!`, which cancelled the in-flight `read_line` every time
    // a command was sent — `read_line` is not cancellation-safe, so this could
    // corrupt the status stream and starve command writes. Splitting them means a
    // command is always written promptly, regardless of read state.
    let read_loop = async {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // service disconnected
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
                }
                Err(e) => {
                    warn!("Pipe read error: {e}");
                    break;
                }
            }
        }
    };

    let write_loop = async {
        while let Some(cmd) = cmd_rx.recv().await {
            let mut json = serde_json::to_string(&cmd).unwrap_or_default();
            json.push('\n');
            if let Err(e) = writer.write_all(json.as_bytes()).await {
                warn!("Pipe write error: {e}");
                break;
            }
            let _ = writer.flush().await;
            info!("Sent command to service: {}", json.trim());
        }
    };

    tokio::select! {
        _ = read_loop => {}
        _ = write_loop => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

    /// A command sent on the cmd channel must be written to the pipe as a JSON
    /// line. Reproduces the UI Approve button → service path.
    #[tokio::test]
    async fn command_is_written_to_pipe() {
        let name = r"\\.\pipe\EirSvcTestClient";
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .pipe_mode(PipeMode::Byte)
            .create(name)
            .expect("create server");

        let status: SharedStatus = Arc::new(Mutex::new(StatusPayload::default()));
        let (tx, rx) = mpsc::channel::<UiMsg>(8);

        let status_c = status.clone();
        let name_owned = name.to_string();
        tokio::spawn(async move { run_on(status_c, rx, &name_owned).await });

        server.connect().await.expect("server accept client");

        // Send an Approve exactly as the Approve button does.
        tx.send(UiMsg::Approve { id: 7, approved: true })
            .await
            .expect("send cmd");

        // The server must receive the JSON line.
        let mut buf = vec![0u8; 256];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), server.read(&mut buf))
            .await
            .expect("server should receive the command, not time out")
            .expect("read");
        let got = String::from_utf8_lossy(&buf[..n]);
        assert!(
            got.contains("\"type\":\"approve\"") && got.contains("\"id\":7"),
            "unexpected payload: {got}"
        );
    }
}
