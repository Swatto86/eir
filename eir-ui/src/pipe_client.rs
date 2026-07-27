use anyhow::Result;
use eir_proto::{CommandResult, ServiceMsg, StatusPayload, UiRequest, PIPE_NAME};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::ClientOptions,
    sync::{mpsc, oneshot},
};
use tracing::{info, warn};

pub type SharedStatus = Arc<Mutex<StatusPayload>>;
pub type CommandWaiters = Arc<Mutex<HashMap<u64, oneshot::Sender<CommandResult>>>>;

/// Lock the shared status, recovering from a poisoned mutex instead of panicking. The
/// guarded data is a plain snapshot the poll/tray/pipe loops clone or overwrite, so a
/// prior panic elsewhere must not cascade into taking those loops down too.
pub fn lock_status(status: &SharedStatus) -> std::sync::MutexGuard<'_, StatusPayload> {
    status.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn lock_waiters(
    waiters: &CommandWaiters,
) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<CommandResult>>> {
    waiters.lock().unwrap_or_else(|p| p.into_inner())
}

/// Runs the pipe client loop forever, reconnecting on disconnect.
/// Updates `status` whenever a StatusPayload arrives from the service, and holds
/// `connected` true only while a live connection exists so command handlers can
/// refuse work that would otherwise be queued and silently dropped.
pub async fn run(
    status: SharedStatus,
    cmd_rx: mpsc::Receiver<UiRequest>,
    connected: Arc<AtomicBool>,
    waiters: CommandWaiters,
) {
    run_on(status, cmd_rx, PIPE_NAME, connected, waiters).await
}

async fn run_on(
    status: SharedStatus,
    mut cmd_rx: mpsc::Receiver<UiRequest>,
    pipe_name: &str,
    connected: Arc<AtomicBool>,
    waiters: CommandWaiters,
) {
    loop {
        let result = connect_and_run(&status, &mut cmd_rx, pipe_name, &connected, &waiters).await;
        // Whatever ended the connection, we are no longer connected — refuse
        // commands until the next connect succeeds.
        connected.store(false, Ordering::Relaxed);
        // Capabilities describe one peer connection. Clear them before reconnecting
        // so a newly connected older service cannot inherit command-result support
        // from the service that just disconnected.
        {
            let mut s = lock_status(&status);
            s.protocol_version = 0;
            s.capabilities.clear();
        }
        match result {
            Ok(()) => {
                // A clean EOF means the service closed the pipe (typically a
                // settings-save restart). Show "reconnecting" instead of leaving the
                // last healthy snapshot on screen; if the service is genuinely down,
                // the next connect attempt fails and surfaces the full error below.
                let mut s = lock_status(&status);
                s.status = "Connecting".to_string();
                s.error = None;
            }
            Err(e) => {
                warn!("Pipe client disconnected: {e}");
                let mut s = lock_status(&status);
                s.status = "ServiceDisconnected".to_string();
                s.error = Some(
                    "Eir service is not running. \
                     Run as Administrator: eir-svc.exe install \
                     then sc start EirSvc"
                        .to_string(),
                );
            }
        }
        // Drop commands that were queued for the connection that just ended — replaying
        // a stale command (e.g. a TogglePause clicked before the drop) on the next
        // connection would act against the user's current intent.
        while cmd_rx.try_recv().is_ok() {}
        // Dropping reply senders wakes any Tauri commands waiting on the dead
        // connection; they fail now rather than sitting on the timeout.
        lock_waiters(&waiters).clear();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_run(
    status: &SharedStatus,
    cmd_rx: &mut mpsc::Receiver<UiRequest>,
    pipe_name: &str,
    connected: &AtomicBool,
    waiters: &CommandWaiters,
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
    connected.store(true, Ordering::Relaxed);

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
                                *lock_status(status) = *payload;
                            }
                            Ok(ServiceMsg::CommandResult(result)) => {
                                if let Some(tx) = lock_waiters(waiters).remove(&result.request_id) {
                                    let _ = tx.send(result);
                                }
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        let (tx, rx) = mpsc::channel::<UiRequest>(8);
        let connected = Arc::new(AtomicBool::new(false));
        let waiters: CommandWaiters = Arc::new(Mutex::new(HashMap::new()));
        let waiters_for_client = waiters.clone();

        let status_c = status.clone();
        let connected_c = connected.clone();
        let name_owned = name.to_string();
        tokio::spawn(async move { run_on(status_c, rx, &name_owned, connected_c, waiters).await });

        server.connect().await.expect("server accept client");

        // Send an Approve exactly as the Approve button does, with a waiter for
        // the service's applied/rejected result.
        let (reply_tx, reply_rx) = oneshot::channel();
        lock_waiters(&waiters_for_client).insert(9, reply_tx);
        tx.send(UiRequest {
            request_id: Some(9),
            command: eir_proto::UiMsg::Approve {
                id: 7,
                approved: true,
            },
        })
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
            got.contains("\"type\":\"approve\"")
                && got.contains("\"id\":7")
                && got.contains("\"request_id\":9"),
            "unexpected payload: {got}"
        );

        let result = CommandResult {
            request_id: 9,
            ok: true,
            message: "Approval applied".to_string(),
        };
        let mut json = serde_json::to_string(&ServiceMsg::CommandResult(result))
            .expect("serialize command result");
        json.push('\n');
        server
            .write_all(json.as_bytes())
            .await
            .expect("write command result");
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx)
            .await
            .expect("client should dispatch command result")
            .expect("waiter remains live");
        assert!(reply.ok);
        assert_eq!(reply.message, "Approval applied");

        // By the time a command has round-tripped, the client is connected: the
        // gate flag must be true so command handlers accept work.
        assert!(
            connected.load(Ordering::Relaxed),
            "connected flag should be true while the pipe is live"
        );

        lock_status(&status)
            .capabilities
            .push(eir_proto::CAP_COMMAND_RESULTS.to_string());
        drop(server);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while connected.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("client should notice the closed pipe");
        assert!(
            lock_status(&status).capabilities.is_empty(),
            "capabilities from the disconnected service must not leak into a new connection"
        );
    }

    #[test]
    fn ensure_connected_gate() {
        let flag = AtomicBool::new(false);
        assert!(crate::ensure_connected(&flag).is_err());
        flag.store(true, Ordering::Relaxed);
        assert!(crate::ensure_connected(&flag).is_ok());
    }
}
