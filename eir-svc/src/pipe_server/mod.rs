//! The service↔UI control-plane server: a Windows named pipe or a Linux Unix-domain
//! socket, both speaking the exact same `UiRequest`/`ServiceMsg`/`CommandResult`/
//! `StatusPayload` JSON-line wire protocol (`eir-proto`) — no wire-format change
//! between platforms. The per-connection body below (reading UiMsg lines, writing
//! Status/CommandResult lines, honouring a liveness watch) is generic over any
//! `AsyncRead + AsyncWrite` stream and is shared unchanged; only accepting a
//! connection and authorising its peer are platform-specific (`windows.rs` session/PID
//! checks, `unix.rs` `SO_PEERCRED`).

use eir_proto::{CommandResult, ServiceMsg, StatusPayload, UiRequest};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc, watch, Notify},
    time::Duration,
};
use tracing::warn;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::current_process_portable_allowed;
#[cfg(windows)]
pub use windows::{spawn, spawn_portable};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{spawn, spawn_portable};

/// Largest UI→service line accepted before the connection is treated as hostile and
/// dropped. Most `UiMsg`s are a few KiB, but an `AskEir` can carry file/image attachments
/// (the tray bounds each image to ~1 MB base64 and text files to a per-file cap); 12 MiB
/// comfortably covers a few attachments while still bounding memory against a
/// misbehaving/malicious client (the pipe/socket is single-client-at-a-time on the
/// Windows named pipe; the Unix socket allows several concurrent clients, each with its
/// own bounded connection).
const MAX_UI_LINE_BYTES: u64 = 12 * 1024 * 1024;
const MAX_SERVICE_LINE_BYTES: usize = 12 * 1024 * 1024;
const MAX_STATUS_PENDING_APPROVALS: usize = 32;
const MAX_RECOVERY_APPROVALS: usize = 8;
const MAX_STATUS_LEARNED_FACTS: usize = 512;
const MAX_STATUS_ACTION_PREFERENCES: usize = 256;

fn finite_f32(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn finite_f64(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn status_for_wire(mut status: StatusPayload) -> StatusPayload {
    // The persistent approval queue remains complete; the UI receives the oldest
    // actionable page, and newer rows appear as those are decided.
    status
        .pending_approvals
        .truncate(MAX_STATUS_PENDING_APPROVALS);
    status.learned_facts.truncate(MAX_STATUS_LEARNED_FACTS);
    status
        .action_preferences
        .truncate(MAX_STATUS_ACTION_PREFERENCES);
    status.cpu = finite_f32(status.cpu);
    status.memory = finite_f32(status.memory);
    status.disk = finite_f32(status.disk);
    for point in &mut status.history {
        point.cpu = finite_f32(point.cpu);
        point.memory = finite_f32(point.memory);
        point.disk = finite_f32(point.disk);
    }
    for problem in &mut status.recent_problems {
        problem.confidence = finite_f32(problem.confidence);
    }
    for approval in &mut status.pending_approvals {
        approval.confidence = finite_f32(approval.confidence);
    }
    if let Some(usage) = &mut status.usage {
        usage.cost_today_usd = finite_f64(usage.cost_today_usd);
        usage.cost_week_usd = finite_f64(usage.cost_week_usd);
    }
    if let Some(settings) = &mut status.settings {
        settings.confidence_threshold = finite_f32(settings.confidence_threshold);
    }
    if let Some(updater) = &mut status.updater {
        updater.last_cost_usd = finite_f64(updater.last_cost_usd);
    }
    if let Some(advisor) = &mut status.advisor {
        advisor.spent_today_usd = finite_f64(advisor.spent_today_usd);
        advisor.settings.low_confidence_threshold =
            finite_f32(advisor.settings.low_confidence_threshold);
    }
    status
}

fn service_line(message: &ServiceMsg) -> Option<Vec<u8>> {
    let mut line = CappedJsonLine::new(MAX_SERVICE_LINE_BYTES - 1);
    match serde_json::to_writer(&mut line, message) {
        Ok(()) => {}
        Err(error) => {
            if line.overflowed {
                warn!("Service message exceeded {MAX_SERVICE_LINE_BYTES} bytes — rejecting frame");
            } else {
                warn!("Could not serialize service message: {error}");
            }
            return None;
        }
    }
    line.bytes.push(b'\n');
    Some(line.bytes)
}

struct CappedJsonLine {
    bytes: Vec<u8>,
    max_bytes: usize,
    overflowed: bool,
}

impl CappedJsonLine {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            overflowed: false,
        }
    }
}

impl std::io::Write for CappedJsonLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.max_bytes {
            self.overflowed = true;
            return Err(std::io::Error::other("JSON line size limit exceeded"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn status_line(status: StatusPayload) -> Option<Vec<u8>> {
    let status = status_for_wire(status);
    let projection = |pending_approvals| StatusPayload {
        protocol_version: status.protocol_version,
        capabilities: eir_proto::service_capabilities(),
        status: "Error".to_string(),
        paused: status.paused,
        cpu: status.cpu,
        memory: status.memory,
        disk: status.disk,
        last_analysis_at: status.last_analysis_at,
        pending_approvals,
        error: Some(
            "Status details were too large to display; oversized details were omitted. \
             The next bounded refresh will restore them."
                .to_string(),
        ),
        gaming: status.gaming,
        signals_at: status.signals_at,
        svc_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        ..StatusPayload::default()
    };
    let recovery = projection(
        status
            .pending_approvals
            .iter()
            .take(MAX_RECOVERY_APPROVALS)
            .cloned()
            .collect(),
    );
    let minimal = projection(Vec::new());
    if let Some(line) = service_line(&ServiceMsg::Status(Box::new(status))) {
        return Some(line);
    }
    warn!("Sending a bounded status error projection");
    service_line(&ServiceMsg::Status(Box::new(recovery)))
        .or_else(|| service_line(&ServiceMsg::Status(Box::new(minimal))))
}

#[derive(Clone)]
pub struct PipeServer {
    status_tx: watch::Sender<StatusPayload>,
    result_tx: broadcast::Sender<ResultEnvelope>,
    /// Number of verified UI/control clients currently connected (Windows: 0 or 1 —
    /// one client at a time; Linux: any number of allowed `eirctl` connections).
    clients: watch::Sender<usize>,
}

#[derive(Clone)]
pub(crate) struct ResultEnvelope {
    result: CommandResult,
    delivered: Option<Arc<Notify>>,
}

impl PipeServer {
    pub fn broadcast_status(&self, status: StatusPayload) {
        // send_replace, not send: `send` discards the value while no client is
        // connected, so the next client would be handed a stale snapshot.
        self.status_tx.send_replace(status);
    }

    /// Whether a verified UI/control client is connected right now.
    pub fn ui_connected(&self) -> bool {
        *self.clients.borrow() > 0
    }

    /// Watch the connected-client count; fires on every connect and disconnect.
    pub fn client_changes(&self) -> watch::Receiver<usize> {
        self.clients.subscribe()
    }

    pub fn command_result(&self, request_id: Option<u64>, result: Result<String, String>) {
        if let Some(envelope) = result_envelope(request_id, result, None) {
            let _ = self.result_tx.send(envelope);
        }
    }

    /// Wait until a connected client has received a result before a caller exits the
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

/// The channel handles a per-platform listener task needs to feed `handle_connection`
/// and accept new UI requests: the `PipeServer` handle to return to the caller, the
/// receiver its `spawn()` hands to the decision loop, and the sender halves of the
/// status/result/command/client-count channels the listener clones per connection.
type ServerChannels = (
    PipeServer,
    mpsc::Receiver<UiRequest>,
    watch::Sender<StatusPayload>,
    broadcast::Sender<ResultEnvelope>,
    mpsc::Sender<UiRequest>,
    watch::Sender<usize>,
);

/// Construct the shared `PipeServer` handle plus the channel its listener task feeds
/// UI requests into. Every platform's `spawn()`/`spawn_portable()` builds these once
/// and hands the sender halves to its own accept loop.
fn new_server() -> ServerChannels {
    let (status_tx, _) = watch::channel(StatusPayload {
        protocol_version: eir_proto::PROTOCOL_VERSION,
        capabilities: eir_proto::service_capabilities(),
        status: "Starting".to_string(),
        ..Default::default()
    });
    let (result_tx, _) = broadcast::channel(32);
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiRequest>(8);
    let (clients_tx, _) = watch::channel(0usize);

    let srv = PipeServer {
        status_tx: status_tx.clone(),
        result_tx: result_tx.clone(),
        clients: clients_tx.clone(),
    };
    (srv, ui_cmd_rx, status_tx, result_tx, ui_cmd_tx, clients_tx)
}

/// Handle one accepted, already-authorised connection: writer task (push the current
/// status immediately, then on every change or correlated command result) plus a
/// reader loop (parse `UiRequest` lines, forward to the decision loop), both gated by
/// `session_rx` — false (or the channel closing) ends the connection. Generic over the
/// transport so the Windows named-pipe and Linux Unix-socket listeners share this
/// entire body verbatim; only how a connection is accepted and authorised differs.
pub(crate) async fn handle_connection<S>(
    stream: S,
    status_tx: watch::Sender<StatusPayload>,
    result_tx: broadcast::Sender<ResultEnvelope>,
    ui_cmd_tx: mpsc::Sender<UiRequest>,
    clients: watch::Sender<usize>,
    session_rx: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    clients.send_modify(|n| *n += 1);

    let (reader, mut writer) = tokio::io::split(stream);
    let mut status_rx = status_tx.subscribe();
    let mut result_rx = result_tx.subscribe();

    // Writer: push current value immediately, then push on every change.
    let mut write_session_rx = session_rx.clone();
    let write_task = tokio::spawn(async move {
        if !*write_session_rx.borrow() {
            return;
        }
        let Some(line) = status_line(status_rx.borrow().clone()) else {
            return;
        };
        if writer.write_all(&line).await.is_err() {
            return;
        }
        if writer.flush().await.is_err() {
            return;
        }

        loop {
            if !*write_session_rx.borrow() {
                break;
            }
            let (line, delivered) = tokio::select! {
                changed = write_session_rx.changed() => {
                    if changed.is_err() || !*write_session_rx.borrow() {
                        break;
                    }
                    continue;
                }
                changed = status_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    (status_line(status_rx.borrow().clone()), None)
                }
                result = result_rx.recv() => {
                    match result {
                        Ok(envelope) => (
                            service_line(&ServiceMsg::CommandResult(envelope.result)),
                            envelope.delivered,
                        ),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            };
            // The connection's authorisation can change while this task is waiting for
            // a status/result (Windows: fast user switching). Re-check immediately
            // before every write so a superseded session cannot receive data after it.
            if !*write_session_rx.borrow() {
                break;
            }
            let Some(line) = line else {
                break;
            };
            if writer.write_all(&line).await.is_err() {
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

    // Reader: process UiMsg lines from the client.
    let mut reader = BufReader::new(reader);
    let mut session_rx = session_rx;
    let mut buf = Vec::new();
    loop {
        // Cap each line: the connected client is still untrusted (beyond the
        // accept-time identity check), so an unbounded read would let it stream bytes
        // with no newline and OOM the service. The largest real UiMsg is a few KiB;
        // anything past the cap is treated as hostile and drops the connection.
        let remaining = MAX_UI_LINE_BYTES.saturating_sub(buf.len() as u64);
        if remaining == 0 {
            warn!("UI message exceeded {MAX_UI_LINE_BYTES} bytes — dropping connection");
            break;
        }
        let mut limited = (&mut reader).take(remaining);
        let read = tokio::select! {
            changed = session_rx.changed() => {
                if changed.is_err() || !*session_rx.borrow() {
                    warn!("Client session is no longer active — disconnecting");
                    break;
                }
                continue;
            }
            read = limited.read_until(b'\n', &mut buf) => read,
        };
        match read {
            Ok(0) => break, // EOF
            Ok(_) if !buf.ends_with(b"\n") => {
                warn!("UI message exceeded {MAX_UI_LINE_BYTES} bytes — dropping connection");
                break;
            }
            Ok(_) => {
                if !*session_rx.borrow() {
                    warn!("Client session is no longer active — disconnecting");
                    break;
                }
                let Ok(text) = std::str::from_utf8(&buf) else {
                    warn!("Bad UI message: invalid UTF-8");
                    buf.clear();
                    continue;
                };
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    buf.clear();
                    continue;
                }
                // All UI messages — including Approve/Reject — are handled by
                // the decision loop, which owns the persistent approval queue.
                match serde_json::from_str::<UiRequest>(trimmed) {
                    Ok(msg) => {
                        let forwarded = tokio::select! {
                            result = ui_cmd_tx.send(msg) => result.is_ok(),
                            _ = session_rx.changed() => false,
                        };
                        if !forwarded || !*session_rx.borrow() {
                            warn!("Client command forwarding was cancelled");
                            break;
                        }
                    }
                    Err(e) => warn!("Bad UI message: {e}"),
                }
                buf.clear();
            }
            Err(e) => {
                warn!("Pipe/socket read error: {e}");
                break;
            }
        }
    }

    write_task.abort();
    let _ = write_task.await;
    clients.send_modify(|n| *n = n.saturating_sub(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_broadcast_with_no_client_connected_reaches_the_next_client() {
        let (srv, _ui_rx, status_tx, _results, _ui_tx, _clients) = new_server();
        srv.broadcast_status(StatusPayload {
            status: "Fresh".to_string(),
            ..Default::default()
        });
        // A client that connects afterwards subscribes and must see the latest value,
        // not the stale one from the last time a client happened to be connected.
        assert_eq!(status_tx.subscribe().borrow().status, "Fresh");
    }

    #[test]
    fn outbound_status_collections_are_bounded_without_mutating_persistent_state() {
        let approval = eir_proto::ApprovalInfo {
            id: 1,
            diagnosis: String::new(),
            root_cause: String::new(),
            confidence: 0.0,
            action: String::new(),
            reason: String::new(),
            side_effects: String::new(),
            undo_instructions: String::new(),
            action_summary: String::new(),
            target: String::new(),
            target_details: String::new(),
            reversible: false,
            created_at: 0,
        };
        let mut original = StatusPayload {
            pending_approvals: vec![approval; MAX_STATUS_PENDING_APPROVALS + 1],
            learned_facts: vec![
                eir_proto::LearnedFactView::default();
                MAX_STATUS_LEARNED_FACTS + 1
            ],
            ..StatusPayload::default()
        };
        original.pending_approvals[MAX_STATUS_PENDING_APPROVALS].id = 999;

        let wire = status_for_wire(original.clone());

        assert_eq!(wire.pending_approvals.len(), MAX_STATUS_PENDING_APPROVALS);
        assert_eq!(wire.learned_facts.len(), MAX_STATUS_LEARNED_FACTS);
        assert_eq!(
            original.pending_approvals.len(),
            MAX_STATUS_PENDING_APPROVALS + 1
        );
        assert_eq!(
            original.pending_approvals[MAX_STATUS_PENDING_APPROVALS].id,
            999
        );
    }

    #[test]
    fn oversized_service_messages_are_not_written() {
        let message = ServiceMsg::CommandResult(CommandResult {
            request_id: 7,
            ok: false,
            message: "x".repeat(MAX_SERVICE_LINE_BYTES),
        });

        assert!(service_line(&message).is_none());
    }

    #[test]
    fn oversized_status_is_replaced_by_a_recoverable_error_projection() {
        let approval = eir_proto::ApprovalInfo {
            id: 42,
            diagnosis: "Needs attention".into(),
            root_cause: String::new(),
            confidence: 0.9,
            action: "restart".into(),
            reason: "approval required".into(),
            side_effects: String::new(),
            undo_instructions: String::new(),
            action_summary: String::new(),
            target: "Spooler".into(),
            target_details: String::new(),
            reversible: true,
            created_at: 0,
        };
        let status = StatusPayload {
            last_analysis: "x".repeat(MAX_SERVICE_LINE_BYTES),
            pending_approvals: vec![approval],
            ..StatusPayload::default()
        };

        let line = status_line(status).expect("bounded status projection");
        assert!(line.len() <= MAX_SERVICE_LINE_BYTES);
        let message: ServiceMsg =
            serde_json::from_slice(&line).expect("projected status is valid JSON");
        let ServiceMsg::Status(status) = message else {
            panic!("status message");
        };
        assert_eq!(status.status, "Error");
        assert!(status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("too large")));
        assert!(status.last_analysis.is_empty());
        assert_eq!(status.pending_approvals.len(), 1);
        assert_eq!(status.pending_approvals[0].id, 42);
    }

    #[test]
    fn pathological_approval_cannot_break_the_minimal_status_projection() {
        let status = StatusPayload {
            pending_approvals: vec![eir_proto::ApprovalInfo {
                id: 42,
                diagnosis: "x".repeat(MAX_SERVICE_LINE_BYTES),
                root_cause: String::new(),
                confidence: 0.9,
                action: String::new(),
                reason: String::new(),
                side_effects: String::new(),
                undo_instructions: String::new(),
                action_summary: String::new(),
                target: String::new(),
                target_details: String::new(),
                reversible: true,
                created_at: 0,
            }],
            ..StatusPayload::default()
        };

        let line = status_line(status).expect("minimal status projection");
        assert!(line.len() <= MAX_SERVICE_LINE_BYTES);
        let message: ServiceMsg =
            serde_json::from_slice(&line).expect("projected status is valid JSON");
        let ServiceMsg::Status(status) = message else {
            panic!("status message");
        };
        assert_eq!(status.status, "Error");
        assert!(status.pending_approvals.is_empty());
    }

    #[test]
    fn nonfinite_numbers_do_not_make_the_status_undecodable() {
        let status = StatusPayload {
            cpu: f32::NAN,
            memory: f32::INFINITY,
            disk: f32::NEG_INFINITY,
            history: vec![eir_proto::MetricPoint {
                at: 1,
                cpu: f32::NAN,
                memory: f32::INFINITY,
                disk: f32::NEG_INFINITY,
            }],
            recent_problems: vec![eir_proto::ProblemSummary {
                diagnosis: String::new(),
                confidence: f32::NAN,
                action: String::new(),
                blocked: false,
                auto_executed: false,
                reason: None,
                at: 0,
            }],
            usage: Some(eir_proto::UsageSummary {
                cost_today_usd: f64::NAN,
                cost_week_usd: f64::INFINITY,
                ..Default::default()
            }),
            settings: Some(eir_proto::UiSettings {
                confidence_threshold: f32::NAN,
                ..Default::default()
            }),
            updater: Some(eir_proto::UpdaterStatus {
                last_cost_usd: f64::INFINITY,
                ..Default::default()
            }),
            advisor: Some(eir_proto::AdvisorStatus {
                spent_today_usd: f64::NAN,
                settings: eir_proto::AdvisorSettingsView {
                    low_confidence_threshold: f32::INFINITY,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..StatusPayload::default()
        };

        let line = status_line(status).expect("status line");
        let message: ServiceMsg =
            serde_json::from_slice(&line).expect("status must round-trip through the UI schema");
        let ServiceMsg::Status(status) = message else {
            panic!("status message");
        };
        assert_eq!((status.cpu, status.memory, status.disk), (0.0, 0.0, 0.0));
        assert_eq!(
            (
                status.history[0].cpu,
                status.history[0].memory,
                status.history[0].disk,
            ),
            (0.0, 0.0, 0.0)
        );
        assert_eq!(status.recent_problems[0].confidence, 0.0);
        let usage = status.usage.expect("usage");
        assert_eq!((usage.cost_today_usd, usage.cost_week_usd), (0.0, 0.0));
        assert_eq!(status.settings.expect("settings").confidence_threshold, 0.0);
        assert_eq!(status.updater.expect("updater").last_cost_usd, 0.0);
        let advisor = status.advisor.expect("advisor");
        assert_eq!(advisor.spent_today_usd, 0.0);
        assert_eq!(advisor.settings.low_confidence_threshold, 0.0);
    }
}
