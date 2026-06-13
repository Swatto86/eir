use sentry_proto::{ServiceMsg, StatusPayload, UiMsg, PIPE_NAME};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::{PipeMode, ServerOptions},
    sync::{mpsc, oneshot, watch},
    time::Duration,
};
use tracing::{info, warn};

type Approvals = Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>;

pub struct PipeServer {
    status_tx: watch::Sender<StatusPayload>,
    approvals: Approvals,
    next_id: Arc<AtomicU64>,
}

pub fn spawn() -> (PipeServer, mpsc::Receiver<UiMsg>) {
    let (status_tx, _) = watch::channel(StatusPayload::default());
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiMsg>(8);
    let approvals: Approvals = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));

    let srv = PipeServer {
        status_tx: status_tx.clone(),
        approvals: approvals.clone(),
        next_id: next_id.clone(),
    };

    tokio::spawn(listener_task(status_tx, ui_cmd_tx, approvals));

    (srv, ui_cmd_rx)
}

async fn listener_task(
    status_tx: watch::Sender<StatusPayload>,
    ui_cmd_tx: mpsc::Sender<UiMsg>,
    approvals: Approvals,
) {
    let mut first = true;
    loop {
        let server = match ServerOptions::new()
            .first_pipe_instance(first)
            .pipe_mode(PipeMode::Byte)
            .create(PIPE_NAME)
        {
            Ok(s) => {
                first = false;
                s
            }
            Err(e) => {
                warn!("Pipe server create error: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        info!("Named pipe listening on {PIPE_NAME}");

        if let Err(e) = server.connect().await {
            warn!("Pipe connect error: {e}");
            continue;
        }

        info!("UI connected to service pipe");

        let (reader, mut writer) = tokio::io::split(server);
        let mut status_rx = status_tx.subscribe();
        let approvals = approvals.clone();
        let ui_cmd_tx = ui_cmd_tx.clone();

        // Writer: push current value immediately, then push on every change.
        let write_task = tokio::spawn(async move {
            // Send current status immediately so the UI gets a snapshot on connect.
            let payload = status_rx.borrow().clone();
            let mut line = serde_json::to_string(&ServiceMsg::Status(payload)).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                return;
            }

            loop {
                if status_rx.changed().await.is_err() {
                    break;
                }
                let payload = status_rx.borrow().clone();
                let mut line =
                    serde_json::to_string(&ServiceMsg::Status(payload)).unwrap_or_default();
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Reader: process UiMsg lines from the UI.
        let mut reader = BufReader::new(reader);
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<UiMsg>(trimmed) {
                        Ok(UiMsg::Approve { id, approved }) => {
                            if let Some(tx) = approvals.lock().unwrap().remove(&id) {
                                let _ = tx.send(approved);
                            }
                        }
                        Ok(msg) => {
                            let _ = ui_cmd_tx.send(msg).await;
                        }
                        Err(e) => warn!("Bad UI message: {e}"),
                    }
                }
                Err(e) => {
                    warn!("Pipe read error: {e}");
                    break;
                }
            }
        }

        write_task.abort();
        info!("UI disconnected from service pipe");
    }
}

impl PipeServer {
    pub fn broadcast_status(&self, status: StatusPayload) {
        let _ = self.status_tx.send(status);
    }

    /// Broadcasts a status containing the approval request, then waits up to 5 minutes
    /// for the UI to respond. Returns `false` on timeout or disconnect.
    pub async fn request_approval(&self, id: u64, status: StatusPayload) -> bool {
        let (tx, rx) = oneshot::channel::<bool>();
        self.approvals.lock().unwrap().insert(id, tx);
        self.broadcast_status(status);

        match tokio::time::timeout(Duration::from_secs(300), rx).await {
            Ok(Ok(approved)) => approved,
            _ => {
                self.approvals.lock().unwrap().remove(&id);
                false
            }
        }
    }

    pub fn next_approval_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}
