use eir_proto::{ServiceMsg, StatusPayload, UiMsg, PIPE_NAME};
use std::ffi::c_void;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::{PipeMode, ServerOptions},
    sync::{mpsc, watch},
    time::Duration,
};
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// Largest UI→service line accepted before the connection is treated as hostile and
/// dropped. The biggest real `UiMsg` (a settings update) is a few KiB; 64 KiB is a
/// comfortable ceiling that still bounds memory against a misbehaving/malicious client.
const MAX_UI_LINE_BYTES: u64 = 64 * 1024;

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

pub struct PipeServer {
    status_tx: watch::Sender<StatusPayload>,
}

pub fn spawn() -> (PipeServer, mpsc::Receiver<UiMsg>) {
    spawn_named(PIPE_NAME)
}

fn spawn_named(pipe_name: &'static str) -> (PipeServer, mpsc::Receiver<UiMsg>) {
    let (status_tx, _) = watch::channel(StatusPayload::default());
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiMsg>(8);

    let srv = PipeServer {
        status_tx: status_tx.clone(),
    };

    tokio::spawn(listener_task(pipe_name, status_tx, ui_cmd_tx));

    (srv, ui_cmd_rx)
}

async fn listener_task(
    pipe_name: &'static str,
    status_tx: watch::Sender<StatusPayload>,
    ui_cmd_tx: mpsc::Sender<UiMsg>,
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
            // Cap each line: the pipe DACL grants Authenticated Users write access, so
            // an unbounded read_line would let any local user stream bytes with no
            // newline and OOM the LocalSystem service. The largest real UiMsg is a few
            // KiB; anything past the cap is treated as hostile and drops the connection.
            let mut limited = (&mut reader).take(MAX_UI_LINE_BYTES);
            match limited.read_line(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) if n as u64 == MAX_UI_LINE_BYTES && !buf.ends_with('\n') => {
                    warn!("UI message exceeded {MAX_UI_LINE_BYTES} bytes — dropping connection");
                    break;
                }
                Ok(_) => {
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // All UI messages — including Approve/Reject — are handled by
                    // the decision loop, which owns the persistent approval queue.
                    match serde_json::from_str::<UiMsg>(trimmed) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ClientOptions;

    /// A client's Approve message is forwarded to the command channel, where the
    /// decision loop resolves it against the persistent queue. Reproduces the
    /// UI → service approval path.
    #[tokio::test]
    async fn approve_message_is_forwarded_to_command_channel() {
        let name = r"\\.\pipe\EirSvcTestApprove";
        let (_srv, mut ui_rx) = spawn_named(name);
        // Let the listener create the pipe instance.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let client = ClientOptions::new().open(name).expect("client connect");
        // Let the listener's connect() return and its read loop start.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Client sends exactly what the UI serialises for UiMsg::Approve.
        let (_r, mut w) = tokio::io::split(client);
        w.write_all(b"{\"type\":\"approve\",\"id\":7,\"approved\":true}\n")
            .await
            .expect("client write");
        w.flush().await.expect("flush");

        let msg = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("Approve should be forwarded, not time out")
            .expect("command channel open");
        match msg {
            UiMsg::Approve { id, approved } => {
                assert_eq!(id, 7);
                assert!(approved);
            }
            other => panic!("expected Approve, got {other:?}"),
        }
    }
}
