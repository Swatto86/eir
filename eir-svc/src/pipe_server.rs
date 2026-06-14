use eir_proto::{ServiceMsg, StatusPayload, UiMsg, PIPE_NAME};
use std::{
    collections::HashMap,
    ffi::c_void,
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
use windows::core::PCWSTR;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// Build a security descriptor that lets the interactive (non-elevated) UI reach
/// the pipe. A pipe created by the LocalSystem service otherwise only grants
/// SYSTEM and Administrators, so the UI fails to open it with "Access is denied".
///
/// Two parts matter:
///  - DACL: SYSTEM + Administrators full control, Authenticated Users read+write
///    (the UI must both receive status and send commands/approvals).
///  - SACL mandatory label set to Medium (`S:(ML;;NW;;;ME)`). Without this the
///    pipe inherits the LocalSystem creator's System integrity, and Windows'
///    no-write-up rule lets the Medium-integrity UI *read* status but silently
///    blocks its *writes* (Approve/Reject/Pause). Labelling the pipe Medium
///    lets the UI write while still blocking Low-integrity (sandboxed) processes.
///
/// Returns the descriptor pointer as a `usize` so it can be carried across the
/// listener's `.await` points (a raw pointer is not `Send`). The descriptor is
/// intentionally leaked so the pointer stays valid for the life of the service.
fn build_pipe_security_descriptor() -> Option<usize> {
    let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)S:(ML;;NW;;;ME)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .ok()?;
    Some(psd.0 as usize)
}

type Approvals = Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>;

pub struct PipeServer {
    status_tx: watch::Sender<StatusPayload>,
    approvals: Approvals,
    next_id: Arc<AtomicU64>,
}

pub fn spawn() -> (PipeServer, mpsc::Receiver<UiMsg>) {
    spawn_named(PIPE_NAME)
}

fn spawn_named(pipe_name: &'static str) -> (PipeServer, mpsc::Receiver<UiMsg>) {
    let (status_tx, _) = watch::channel(StatusPayload::default());
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiMsg>(8);
    let approvals: Approvals = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));

    let srv = PipeServer {
        status_tx: status_tx.clone(),
        approvals: approvals.clone(),
        next_id: next_id.clone(),
    };

    tokio::spawn(listener_task(pipe_name, status_tx, ui_cmd_tx, approvals));

    (srv, ui_cmd_rx)
}

async fn listener_task(
    pipe_name: &'static str,
    status_tx: watch::Sender<StatusPayload>,
    ui_cmd_tx: mpsc::Sender<UiMsg>,
    approvals: Approvals,
) {
    let sd_ptr = build_pipe_security_descriptor();
    if sd_ptr.is_none() {
        warn!("Could not build pipe security descriptor; the UI may be unable to connect");
    }

    let mut first = true;
    loop {
        // Construct the SECURITY_ATTRIBUTES in a scope that ends before the first
        // .await below, so the non-Send raw pointer is never held across an await.
        let created = {
            let mut opts = ServerOptions::new();
            opts.first_pipe_instance(first).pipe_mode(PipeMode::Byte);
            match sd_ptr {
                Some(p) => {
                    let mut sa = SECURITY_ATTRIBUTES {
                        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                        lpSecurityDescriptor: p as *mut c_void,
                        bInheritHandle: false.into(),
                    };
                    unsafe {
                        opts.create_with_security_attributes_raw(
                            pipe_name,
                            (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
                        )
                    }
                }
                None => opts.create(pipe_name),
            }
        };
        let server = match created {
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

        info!("Named pipe listening on {pipe_name}");

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ClientOptions;

    /// End-to-end: a client connected to the server pipe can approve a pending
    /// request and unblock `request_approval`. Reproduces the UI → service path.
    #[tokio::test]
    async fn approve_message_resolves_request() {
        let name = r"\\.\pipe\EirSvcTestApprove";
        let (srv, _ui_rx) = spawn_named(name);
        // Let the listener create the pipe instance.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let client = ClientOptions::new().open(name).expect("client connect");
        // Let the listener's connect() return and its read loop start.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Service awaits approval for id = 7.
        let handle =
            tokio::spawn(async move { srv.request_approval(7, StatusPayload::default()).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client sends exactly what the UI serialises for UiMsg::Approve.
        let (_r, mut w) = tokio::io::split(client);
        w.write_all(b"{\"type\":\"approve\",\"id\":7,\"approved\":true}\n")
            .await
            .expect("client write");
        w.flush().await.expect("flush");

        let approved = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("request_approval should resolve, not time out")
            .expect("task join");
        assert!(approved, "approval should be true");
    }
}
