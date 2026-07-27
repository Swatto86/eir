use eir_proto::{
    CommandResult, ServiceMsg, StatusPayload, UiRequest, CAP_COMMAND_RESULTS, CAP_PROVIDER_TEST,
    PIPE_NAME, PROTOCOL_VERSION,
};
use std::{ffi::c_void, os::windows::io::AsRawHandle, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::{PipeMode, ServerOptions},
    sync::{broadcast, mpsc, watch, Notify},
    time::Duration,
};
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};

/// Largest UI→service line accepted before the connection is treated as hostile and
/// dropped. Most `UiMsg`s are a few KiB, but an `AskEir` can carry file/image attachments
/// (the tray bounds each image to ~1 MB base64 and text files to a per-file cap); 12 MiB
/// comfortably covers a few attachments while still bounding memory against a
/// misbehaving/malicious client (the pipe is single-client, one line at a time).
const MAX_UI_LINE_BYTES: u64 = 12 * 1024 * 1024;

/// Build a security descriptor that lets the interactive (non-elevated) UI reach
/// the pipe. A pipe created by the LocalSystem service otherwise only grants
/// SYSTEM and Administrators, so the UI fails to open it with "Access is denied".
///
/// Two parts matter:
///  - DACL: SYSTEM + Administrators full control, Interactive Users read+write
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
    let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)S:(ML;;NW;;;ME)"
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

#[derive(Clone)]
pub struct PipeServer {
    status_tx: watch::Sender<StatusPayload>,
    result_tx: broadcast::Sender<ResultEnvelope>,
}

#[derive(Clone)]
struct ResultEnvelope {
    result: CommandResult,
    delivered: Option<Arc<Notify>>,
}

pub fn spawn() -> (PipeServer, mpsc::Receiver<UiRequest>) {
    spawn_named(PIPE_NAME)
}

fn spawn_named(pipe_name: &'static str) -> (PipeServer, mpsc::Receiver<UiRequest>) {
    let (status_tx, _) = watch::channel(StatusPayload {
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec![
            CAP_COMMAND_RESULTS.to_string(),
            CAP_PROVIDER_TEST.to_string(),
        ],
        status: "Starting".to_string(),
        ..Default::default()
    });
    let (result_tx, _) = broadcast::channel(32);
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiRequest>(8);

    let srv = PipeServer {
        status_tx: status_tx.clone(),
        result_tx: result_tx.clone(),
    };

    tokio::spawn(listener_task(pipe_name, status_tx, result_tx, ui_cmd_tx));

    (srv, ui_cmd_rx)
}

async fn listener_task(
    pipe_name: &'static str,
    status_tx: watch::Sender<StatusPayload>,
    result_tx: broadcast::Sender<ResultEnvelope>,
    ui_cmd_tx: mpsc::Sender<UiRequest>,
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

        let Some(client_session) = pipe_client_session(&server) else {
            warn!("Could not identify pipe client session");
            continue;
        };
        if !client_session_allowed(client_session, unsafe { WTSGetActiveConsoleSessionId() }) {
            warn!("Rejected pipe client outside the active interactive session");
            continue;
        }

        info!("UI connected to service pipe");

        let (reader, mut writer) = tokio::io::split(server);
        let mut status_rx = status_tx.subscribe();
        let mut result_rx = result_tx.subscribe();
        let ui_cmd_tx = ui_cmd_tx.clone();

        // Writer: push current value immediately, then push on every change.
        let write_task = tokio::spawn(async move {
            // Send current status immediately so the UI gets a snapshot on connect.
            if !client_session_allowed(client_session, unsafe { WTSGetActiveConsoleSessionId() }) {
                return;
            }
            let payload = status_rx.borrow().clone();
            let mut line =
                serde_json::to_string(&ServiceMsg::Status(Box::new(payload))).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }

            loop {
                if !client_session_allowed(client_session, unsafe {
                    WTSGetActiveConsoleSessionId()
                }) {
                    break;
                }
                let (message, delivered) = tokio::select! {
                    changed = status_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        (ServiceMsg::Status(Box::new(status_rx.borrow().clone())), None)
                    }
                    result = result_rx.recv() => {
                        match result {
                            Ok(envelope) => (
                                ServiceMsg::CommandResult(envelope.result),
                                envelope.delivered,
                            ),
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                };
                // The active desktop can change while this task is waiting for a
                // status/result. Re-check immediately before every write so the
                // prior session cannot receive data after fast user switching.
                if !client_session_allowed(client_session, unsafe {
                    WTSGetActiveConsoleSessionId()
                }) {
                    break;
                }
                let mut line = serde_json::to_string(&message).unwrap_or_default();
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
                if let Some(delivered) = delivered {
                    delivered.notify_one();
                }
            }
        });

        // Reader: process UiMsg lines from the UI.
        let mut reader = BufReader::new(reader);
        let mut buf = String::new();
        loop {
            buf.clear();
            // Cap each line: the active interactive client is still untrusted, so an
            // unbounded read_line would let it stream bytes with no
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
                    if !client_session_allowed(client_session, unsafe {
                        WTSGetActiveConsoleSessionId()
                    }) {
                        warn!("Pipe client session is no longer active — disconnecting");
                        break;
                    }
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // All UI messages — including Approve/Reject — are handled by
                    // the decision loop, which owns the persistent approval queue.
                    match serde_json::from_str::<UiRequest>(trimmed) {
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

    pub fn command_result(&self, request_id: Option<u64>, result: Result<String, String>) {
        if let Some(envelope) = result_envelope(request_id, result, None) {
            let _ = self.result_tx.send(envelope);
        }
    }

    /// Wait until the connected UI has received a result before a caller exits the
    /// service runtime (the settings-restart path).
    pub async fn command_result_flushed(
        &self,
        request_id: Option<u64>,
        result: Result<String, String>,
    ) -> bool {
        let Some(request_id) = request_id else {
            return true;
        };
        let delivered = Arc::new(Notify::new());
        let Some(envelope) = result_envelope(Some(request_id), result, Some(delivered.clone()))
        else {
            return true;
        };
        if self.result_tx.send(envelope).is_err() {
            return false;
        }
        tokio::time::timeout(Duration::from_secs(5), delivered.notified())
            .await
            .is_ok()
    }
}

fn result_envelope(
    request_id: Option<u64>,
    result: Result<String, String>,
    delivered: Option<Arc<Notify>>,
) -> Option<ResultEnvelope> {
    let request_id = request_id?;
    let (ok, message) = match result {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    Some(ResultEnvelope {
        result: CommandResult {
            request_id,
            ok,
            message,
        },
        delivered,
    })
}

fn client_session_allowed(client_session: u32, active_session: u32) -> bool {
    client_session != u32::MAX && active_session != u32::MAX && client_session == active_session
}

fn pipe_client_session(server: &tokio::net::windows::named_pipe::NamedPipeServer) -> Option<u32> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetNamedPipeClientProcessId(pipe: *mut c_void, client_process_id: *mut u32) -> i32;
    }

    let mut process_id = 0;
    let got_pid =
        unsafe { GetNamedPipeClientProcessId(server.as_raw_handle().cast(), &mut process_id) };
    if got_pid == 0 {
        return None;
    }
    let mut client_session = u32::MAX;
    if unsafe { ProcessIdToSessionId(process_id, &mut client_session) }.is_err() {
        return None;
    }
    Some(client_session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ClientOptions;

    #[test]
    fn only_the_active_interactive_session_is_accepted() {
        let connected_session = 4;
        assert!(client_session_allowed(connected_session, 4));
        // The same established client is rejected after fast-user-switching.
        assert!(!client_session_allowed(connected_session, 5));
        assert!(!client_session_allowed(3, 4));
        assert!(!client_session_allowed(u32::MAX, 4));
        assert!(!client_session_allowed(4, u32::MAX));
    }

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
        match msg.command {
            eir_proto::UiMsg::Approve { id, approved } => {
                assert_eq!(id, 7);
                assert!(approved);
            }
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn correlated_command_result_is_written_to_the_client() {
        let name = r"\\.\pipe\EirSvcTestResult";
        let (srv, mut ui_rx) = spawn_named(name);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let client = ClientOptions::new().open(name).expect("client connect");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (reader, mut writer) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        writer
            .write_all(b"{\"type\":\"approve\",\"id\":7,\"approved\":false,\"request_id\":42}\n")
            .await
            .expect("client write");
        let request = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("request timeout")
            .expect("request");
        assert_eq!(request.request_id, Some(42));

        let mut line = String::new();
        reader.read_line(&mut line).await.expect("initial status");
        match serde_json::from_str::<ServiceMsg>(line.trim()).expect("initial status message") {
            ServiceMsg::Status(status) => {
                assert_eq!(status.protocol_version, PROTOCOL_VERSION);
                assert!(status.capabilities.iter().any(|x| x == CAP_COMMAND_RESULTS));
            }
            other => panic!("expected status, got {other:?}"),
        }
        line.clear();
        let (delivered, read) = tokio::join!(
            srv.command_result_flushed(Some(42), Ok("Action rejected".to_string())),
            reader.read_line(&mut line)
        );
        assert!(delivered);
        read.expect("command result");
        match serde_json::from_str::<ServiceMsg>(line.trim()).expect("service message") {
            ServiceMsg::CommandResult(result) => {
                assert_eq!(result.request_id, 42);
                assert!(result.ok);
                assert_eq!(result.message, "Action rejected");
            }
            other => panic!("expected command result, got {other:?}"),
        }
    }
}
