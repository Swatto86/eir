use eir_proto::{CommandResult, ServiceMsg, StatusPayload, UiRequest, PIPE_NAME, PROTOCOL_VERSION};
use std::{
    ffi::{c_void, OsString},
    os::windows::{ffi::OsStringExt, io::AsRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::{PipeMode, ServerOptions},
    sync::{broadcast, mpsc, watch, Notify},
    time::Duration,
};
use tracing::{info, warn};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, DuplicateToken, EqualSid, GetTokenInformation,
    SecurityIdentification, TokenElevation, TokenElevationType, TokenElevationTypeDefault,
    TokenElevationTypeLimited, TokenSessionId, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE,
    TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
#[cfg(test)]
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Largest UI→service line accepted before the connection is treated as hostile and
/// dropped. Most `UiMsg`s are a few KiB, but an `AskEir` can carry file/image attachments
/// (the tray bounds each image to ~1 MB base64 and text files to a per-file cap); 12 MiB
/// comfortably covers a few attachments while still bounding memory against a
/// misbehaving/malicious client (the pipe is single-client, one line at a time).
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

/// FILE_READ_DATA | FILE_WRITE_DATA. Unlike FILE_GENERIC_WRITE, this does not contain
/// FILE_CREATE_PIPE_INSTANCE, so an interactive client can exchange data without
/// creating a competing server instance.
#[cfg(test)]
const INTERACTIVE_PIPE_ACCESS: u32 = 0x0012_008B;
const PIPE_SDDL: &str =
    "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x0012008b;;;IU)S:(ML;;NWNR;;;ME)";

/// Build a security descriptor that lets the interactive (non-elevated) UI reach
/// the pipe. A pipe created by the LocalSystem service otherwise only grants
/// SYSTEM and Administrators, so the UI fails to open it with "Access is denied".
///
/// Two parts matter:
///  - DACL: SYSTEM + Administrators + the object owner have full control;
///    Interactive Users get client-only read+write rights. Owner Rights matters
///    for portable mode, where the unelevated creator must create later server
///    instances after a client disconnects.
///  - SACL mandatory label set to Medium (`S:(ML;;NWNR;;;ME)`). Without this the
///    pipe inherits the LocalSystem creator's System integrity, and Windows'
///    no-write-up rule lets the Medium-integrity UI *read* status but silently
///    blocks its *writes* (Approve/Reject/Pause). Labelling the pipe Medium
///    lets the UI read/write while blocking Low-integrity (sandboxed) processes
///    from both reading status and sending commands.
///
/// Returns the descriptor pointer as a `usize` so it can be carried across the
/// listener's `.await` points (a raw pointer is not `Send`). The descriptor is
/// intentionally leaked so the pointer stays valid for the life of the service.
fn build_pipe_security_descriptor() -> Option<usize> {
    let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain(std::iter::once(0)).collect();
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

type ClientVerifier = fn(u32) -> Option<u32>;
type SessionVerifier = fn(u32) -> bool;

pub fn spawn() -> (PipeServer, mpsc::Receiver<UiRequest>) {
    spawn_named_with_verifier(
        PIPE_NAME.to_string(),
        verified_client_session,
        sole_active_client_session,
    )
}

pub fn spawn_portable(pipe_name: String) -> (PipeServer, mpsc::Receiver<UiRequest>) {
    spawn_named_with_verifier(
        pipe_name,
        verified_portable_client_session,
        sole_active_client_session,
    )
}

#[cfg(test)]
fn spawn_named(pipe_name: &'static str) -> (PipeServer, mpsc::Receiver<UiRequest>) {
    spawn_named_with_verifier(
        pipe_name.to_string(),
        test_client_session,
        test_session_allowed,
    )
}

fn spawn_named_with_verifier(
    pipe_name: String,
    verify_client: ClientVerifier,
    verify_session: SessionVerifier,
) -> (PipeServer, mpsc::Receiver<UiRequest>) {
    let (status_tx, _) = watch::channel(StatusPayload {
        protocol_version: PROTOCOL_VERSION,
        capabilities: eir_proto::service_capabilities(),
        status: "Starting".to_string(),
        ..Default::default()
    });
    let (result_tx, _) = broadcast::channel(32);
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiRequest>(8);

    let srv = PipeServer {
        status_tx: status_tx.clone(),
        result_tx: result_tx.clone(),
    };

    tokio::spawn(listener_task(
        pipe_name,
        status_tx,
        result_tx,
        ui_cmd_tx,
        verify_client,
        verify_session,
    ));

    (srv, ui_cmd_rx)
}

async fn listener_task(
    pipe_name: String,
    status_tx: watch::Sender<StatusPayload>,
    result_tx: broadcast::Sender<ResultEnvelope>,
    ui_cmd_tx: mpsc::Sender<UiRequest>,
    verify_client: ClientVerifier,
    verify_session: SessionVerifier,
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
                            &pipe_name,
                            (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
                        )
                    }
                }
                None => opts.create(&pipe_name),
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

        let Some(client_pid) = pipe_client_process_id(&server) else {
            warn!("Could not identify pipe client process");
            continue;
        };
        let Some(client_session) = verify_client_off_runtime(verify_client, client_pid).await
        else {
            warn!("Rejected pipe client that is not a trusted Eir UI or elevated administrator");
            continue;
        };
        if !verify_session_off_runtime(verify_session, client_session).await {
            warn!("Rejected pipe client outside the active interactive session");
            continue;
        }

        info!("UI connected to service pipe");

        let (reader, mut writer) = tokio::io::split(server);
        let mut status_rx = status_tx.subscribe();
        let mut result_rx = result_tx.subscribe();
        let ui_cmd_tx = ui_cmd_tx.clone();
        let (session_tx, session_rx) = watch::channel(true);
        let session_watch_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.tick().await; // the connection was checked immediately above
            loop {
                ticker.tick().await;
                if !verify_session_off_runtime(verify_session, client_session).await {
                    let _ = session_tx.send(false);
                    break;
                }
            }
        });

        // Writer: push current value immediately, then push on every change.
        let mut write_session_rx = session_rx.clone();
        let write_task = tokio::spawn(async move {
            // Send current status immediately so the UI gets a snapshot on connect.
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
                // The active desktop can change while this task is waiting for a
                // status/result. Re-check immediately before every write so the
                // prior session cannot receive data after fast user switching.
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

        // Reader: process UiMsg lines from the UI.
        let mut reader = BufReader::new(reader);
        let mut session_rx = session_rx;
        let mut buf = Vec::new();
        loop {
            // Cap each line: the active interactive client is still untrusted, so an
            // unbounded read would let it stream bytes with no
            // newline and OOM the LocalSystem service. The largest real UiMsg is a few
            // KiB; anything past the cap is treated as hostile and drops the connection.
            let remaining = MAX_UI_LINE_BYTES.saturating_sub(buf.len() as u64);
            if remaining == 0 {
                warn!("UI message exceeded {MAX_UI_LINE_BYTES} bytes — dropping connection");
                break;
            }
            let mut limited = (&mut reader).take(remaining);
            let read = tokio::select! {
                changed = session_rx.changed() => {
                    if changed.is_err() || !*session_rx.borrow() {
                        warn!("Pipe client session is no longer active — disconnecting");
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
                        warn!("Pipe client session is no longer active — disconnecting");
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
                                warn!("Pipe client command forwarding was cancelled");
                                break;
                            }
                        }
                        Err(e) => warn!("Bad UI message: {e}"),
                    }
                    buf.clear();
                }
                Err(e) => {
                    warn!("Pipe read error: {e}");
                    break;
                }
            }
        }

        write_task.abort();
        let _ = write_task.await;
        session_watch_task.abort();
        let _ = session_watch_task.await;
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

fn client_session_matches_active(client_session: u32, active_session: Option<u32>) -> bool {
    client_session != u32::MAX && active_session == Some(client_session)
}

fn sole_active_client_session(client_session: u32) -> bool {
    client_session_matches_active(client_session, crate::session::active_user_session_id())
}

#[cfg(test)]
fn test_session_allowed(_: u32) -> bool {
    true
}

fn client_identity_evidence_allowed(
    session_active: bool,
    elevated_admin: bool,
    installed_ui_path: bool,
) -> bool {
    session_active && (elevated_admin || installed_ui_path)
}

async fn verify_client_off_runtime(verify_client: ClientVerifier, process_id: u32) -> Option<u32> {
    match tokio::task::spawn_blocking(move || verify_client(process_id)).await {
        Ok(session) => session,
        Err(error) => {
            warn!("Pipe client identity check failed: {error}");
            None
        }
    }
}

async fn verify_session_off_runtime(verify_session: SessionVerifier, session_id: u32) -> bool {
    tokio::task::spawn_blocking(move || verify_session(session_id))
        .await
        .unwrap_or(false)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
fn process_session_id(process_id: u32) -> Option<u32> {
    let mut session = u32::MAX;
    unsafe { ProcessIdToSessionId(process_id, &mut session) }
        .ok()
        .map(|()| session)
}

fn process_token_session_id(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let token = OwnedHandle(token);
    let mut session = u32::MAX;
    let mut returned = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenSessionId,
            Some((&mut session as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        )
    }
    .ok()?;
    (returned as usize >= std::mem::size_of::<u32>() && session != u32::MAX).then_some(session)
}

#[cfg(test)]
fn test_client_session(process_id: u32) -> Option<u32> {
    process_session_id(process_id)
}

/// Authorise a pipe client while holding its process handle open, preventing PID reuse
/// between the session and image checks. Elevated Administrators already control the
/// service through SCM and are accepted for maintenance/CI. A normal user must be
/// running the installed UI beside the service. Portable binaries deliberately fail
/// closed: hashing their current pathname cannot prove which bytes the process mapped,
/// because a user can replace that file after launching a different executable.
fn verified_client_session(process_id: u32) -> Option<u32> {
    let process = OwnedHandle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?,
    );
    let session = process_token_session_id(process.0)?;
    let session_active = sole_active_client_session(session);
    let elevated_admin = process_is_elevated_admin(process.0);
    if client_identity_evidence_allowed(session_active, elevated_admin, false) {
        return Some(session);
    }

    let image = process_image_path(process.0)?;
    let image = checked_fixed_image_path(&image);
    let installed = installed_ui_image_path();
    let same_path = image
        .as_ref()
        .zip(installed.as_ref())
        .is_some_and(|(image, installed)| windows_path_eq(image, installed));
    client_identity_evidence_allowed(session_active, false, same_path).then_some(session)
}

fn verified_portable_client_session(process_id: u32) -> Option<u32> {
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
    {
        Ok(process) => OwnedHandle(process),
        Err(error) => {
            warn!(%error, "Rejected portable UI: cannot open process");
            return None;
        }
    };
    let Some(session) = process_token_session_id(process.0) else {
        warn!("Rejected portable UI: cannot read token session");
        return None;
    };
    if !processes_share_user(process.0, unsafe { GetCurrentProcess() }) {
        warn!("Rejected portable UI: token user differs from foreground service");
        return None;
    }
    let Some(image) =
        process_image_path(process.0).and_then(|path| checked_fixed_image_path(&path))
    else {
        warn!("Rejected portable UI: image path is not a regular local file");
        return None;
    };
    let Some(expected) = std::env::current_exe()
        .ok()
        .and_then(|service| portable_ui_image_path_at(&service))
    else {
        warn!("Rejected portable UI: sibling UI path is not a regular local file");
        return None;
    };
    if !windows_path_eq(&image, &expected) {
        warn!(
            image = %image.display(),
            expected = %expected.display(),
            "Rejected portable UI: image path mismatch"
        );
        return None;
    }
    Some(session)
}

fn portable_ui_image_path_at(service: &Path) -> Option<PathBuf> {
    checked_fixed_image_path(&service.parent()?.join("eir.exe"))
}

fn token_user_buffer(token: HANDLE) -> Option<Vec<usize>> {
    let mut bytes = 0;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut bytes) };
    if bytes < std::mem::size_of::<TOKEN_USER>() as u32 {
        return None;
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
    }
    .ok()?;
    Some(buffer)
}

fn processes_share_user(left: HANDLE, right: HANDLE) -> bool {
    let mut left_token = HANDLE::default();
    let mut right_token = HANDLE::default();
    if unsafe { OpenProcessToken(left, TOKEN_QUERY, &mut left_token) }.is_err()
        || unsafe { OpenProcessToken(right, TOKEN_QUERY, &mut right_token) }.is_err()
    {
        if !left_token.is_invalid() {
            drop(OwnedHandle(left_token));
        }
        return false;
    }
    let left_token = OwnedHandle(left_token);
    let right_token = OwnedHandle(right_token);
    let Some(left_user) = token_user_buffer(left_token.0) else {
        return false;
    };
    let Some(right_user) = token_user_buffer(right_token.0) else {
        return false;
    };
    let left_user = unsafe { &*left_user.as_ptr().cast::<TOKEN_USER>() };
    let right_user = unsafe { &*right_user.as_ptr().cast::<TOKEN_USER>() };
    unsafe { EqualSid(left_user.User.Sid, right_user.User.Sid) }.is_ok()
}

fn process_is_elevated_admin(process: HANDLE) -> bool {
    let mut token = HANDLE::default();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) }.is_err() {
        return false;
    }
    let token = OwnedHandle(token);
    token_is_elevated(token.0).unwrap_or(false) && token_admin_membership(token.0).unwrap_or(false)
}

fn token_is_elevated(token: HANDLE) -> Option<bool> {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .ok()?;
    (returned as usize >= std::mem::size_of::<TOKEN_ELEVATION>())
        .then_some(elevation.TokenIsElevated != 0)
}

fn token_elevation_type(token: HANDLE) -> Option<TOKEN_ELEVATION_TYPE> {
    let mut elevation_type = TOKEN_ELEVATION_TYPE::default();
    let mut returned = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            Some((&mut elevation_type as *mut TOKEN_ELEVATION_TYPE).cast()),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut returned,
        )
    }
    .ok()?;
    (returned as usize >= std::mem::size_of::<TOKEN_ELEVATION_TYPE>()).then_some(elevation_type)
}

fn token_is_local_system(token: HANDLE) -> Option<bool> {
    let user = token_user_buffer(token)?;
    let user = unsafe { &*user.as_ptr().cast::<TOKEN_USER>() };
    let mut sid =
        vec![0usize; (SECURITY_MAX_SID_SIZE as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut sid_bytes = SECURITY_MAX_SID_SIZE;
    let sid = PSID(sid.as_mut_ptr().cast());
    unsafe { CreateWellKnownSid(WinLocalSystemSid, PSID::default(), sid, &mut sid_bytes) }.ok()?;
    Some(unsafe { EqualSid(user.User.Sid, sid) }.is_ok())
}

fn portable_token_identity_allowed(
    elevation_type: Option<TOKEN_ELEVATION_TYPE>,
    local_system: Option<bool>,
) -> bool {
    local_system == Some(false)
        && matches!(
            elevation_type,
            Some(value)
                if value == TokenElevationTypeDefault || value == TokenElevationTypeLimited
        )
}

pub(crate) fn current_process_portable_allowed() -> bool {
    let mut token = HANDLE::default();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let token = OwnedHandle(token);
    portable_token_identity_allowed(
        token_elevation_type(token.0),
        token_is_local_system(token.0),
    )
}

fn token_admin_membership(primary_token: HANDLE) -> Option<bool> {
    let mut impersonation_token = HANDLE::default();
    unsafe {
        DuplicateToken(
            primary_token,
            SecurityIdentification,
            &mut impersonation_token,
        )
    }
    .ok()?;
    let impersonation_token = OwnedHandle(impersonation_token);

    let words = (SECURITY_MAX_SID_SIZE as usize).div_ceil(std::mem::size_of::<usize>());
    let mut sid_buffer = vec![0usize; words];
    let mut sid_bytes = (sid_buffer.len() * std::mem::size_of::<usize>()) as u32;
    let sid = PSID(sid_buffer.as_mut_ptr().cast());
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            PSID::default(),
            sid,
            &mut sid_bytes,
        )
    }
    .is_err()
    {
        return None;
    }
    let mut is_admin = BOOL::default();
    unsafe { CheckTokenMembership(impersonation_token.0, sid, &mut is_admin) }
        .ok()
        .map(|()| is_admin.as_bool())
}

fn process_image_path(process: HANDLE) -> Option<PathBuf> {
    let mut image = vec![0u16; 32_768];
    let mut chars = image.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &mut chars,
        )
    }
    .ok()?;
    image.truncate(chars as usize);
    Some(PathBuf::from(OsString::from_wide(&image)))
}

fn fixed_drive_root(path: &Path) -> Option<[u16; 4]> {
    let mut components = path.components();
    let letter = match components.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) => letter,
            _ => return None,
        },
        _ => return None,
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return None;
    }
    Some([u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0])
}

fn checked_fixed_image_path(path: &Path) -> Option<PathBuf> {
    let root = fixed_drive_root(path)?;
    // DRIVE_FIXED = 3. Reject mapped/network/removable roots before any filesystem
    // inspection so LocalSystem never authenticates to an attacker-controlled share.
    if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != 3 {
        return None;
    }
    // Reuse the file-action boundary: it rejects every reparse component before
    // canonicalising, so an untrusted executable path cannot redirect this SYSTEM read.
    crate::executor::logs::checked_local_path(path)
        .ok()
        .flatten()
}

fn installed_ui_image_path() -> Option<PathBuf> {
    let service = std::env::current_exe().ok()?;
    checked_fixed_image_path(&service.parent()?.join("eir.exe"))
}

fn windows_path_eq(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn pipe_client_process_id(
    server: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> Option<u32> {
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
    Some(process_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::windows::named_pipe::NamedPipeClient;

    static TEST_SESSION_ACTIVE: AtomicBool = AtomicBool::new(true);
    static TEST_QUEUE_SESSION_ACTIVE: AtomicBool = AtomicBool::new(true);

    #[test]
    fn portable_token_policy_rejects_split_elevation_and_local_system() {
        assert!(portable_token_identity_allowed(
            Some(TokenElevationTypeDefault),
            Some(false)
        ));
        assert!(portable_token_identity_allowed(
            Some(TokenElevationTypeLimited),
            Some(false)
        ));
        assert!(!portable_token_identity_allowed(
            Some(windows::Win32::Security::TokenElevationTypeFull),
            Some(false)
        ));
        assert!(!portable_token_identity_allowed(
            Some(TokenElevationTypeDefault),
            Some(true)
        ));
        assert!(!portable_token_identity_allowed(None, Some(false)));
    }

    #[test]
    fn portable_ui_path_is_derived_before_canonicalisation() {
        let root =
            std::env::temp_dir().join(format!("eir-portable-ui-path-{}", std::process::id()));
        let service = root.join("eir-svc.exe");
        let ui = root.join("eir.exe");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create portable path test directory");
        std::fs::write(&service, b"service").expect("write test service");
        std::fs::write(&ui, b"ui").expect("write test UI");

        assert_eq!(
            portable_ui_image_path_at(&service),
            std::fs::canonicalize(&ui).ok()
        );

        std::fs::remove_dir_all(root).expect("remove portable path test directory");
    }

    fn toggled_session_allowed(_: u32) -> bool {
        TEST_SESSION_ACTIVE.load(Ordering::Relaxed)
    }

    fn queued_session_allowed(_: u32) -> bool {
        TEST_QUEUE_SESSION_ACTIVE.load(Ordering::Relaxed)
    }

    fn open_test_pipe(name: &str) -> std::io::Result<NamedPipeClient> {
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_SHARE_MODE, FILE_WRITE_DATA,
            OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        };

        let path: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_READ_DATA.0 | FILE_WRITE_DATA.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT,
                None,
            )
        }
        .map_err(|error| {
            std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xFFFF) as i32)
        })?;
        unsafe { NamedPipeClient::from_raw_handle(handle.0) }
    }

    fn slow_client_verifier(_: u32) -> Option<u32> {
        std::thread::sleep(Duration::from_millis(100));
        Some(7)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_identity_checks_do_not_block_the_async_listener() {
        let verification = verify_client_off_runtime(slow_client_verifier, 1);
        tokio::pin!(verification);

        tokio::select! {
            result = &mut verification => panic!("blocking verification ran on the runtime: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        assert_eq!(verification.await, Some(7));
    }

    #[test]
    fn administrator_membership_accepts_a_primary_process_token() {
        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(
                windows::Win32::System::Threading::GetCurrentProcess(),
                TOKEN_QUERY | windows::Win32::Security::TOKEN_DUPLICATE,
                &mut token,
            )
        }
        .expect("open current process token");
        let token = OwnedHandle(token);
        assert!(
            token_admin_membership(token.0).is_some(),
            "membership check must duplicate the primary process token before querying it"
        );
    }

    #[test]
    fn process_session_is_read_from_the_held_process_handle() {
        let process = OwnedHandle(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, std::process::id()) }
                .expect("open current process"),
        );
        assert_eq!(
            process_token_session_id(process.0),
            process_session_id(std::process::id())
        );
    }

    #[test]
    fn only_the_active_interactive_session_is_accepted() {
        assert!(client_session_matches_active(2, Some(2)));
        assert!(!client_session_matches_active(2, Some(3)));
        // None is the fail-closed result when zero or multiple sessions are active.
        assert!(!client_session_matches_active(2, None));
        assert!(!client_session_matches_active(u32::MAX, Some(2)));
    }

    #[test]
    fn low_integrity_clients_cannot_read_or_write_the_pipe() {
        assert!(
            PIPE_SDDL.contains("NWNR"),
            "the mandatory label must deny both read-up and write-up"
        );
    }

    #[test]
    fn interactive_pipe_rights_cannot_create_competing_server_instances() {
        const FILE_CREATE_PIPE_INSTANCE: u32 = 0x0004;
        assert_eq!(INTERACTIVE_PIPE_ACCESS & FILE_CREATE_PIPE_INSTANCE, 0);
        assert_ne!(INTERACTIVE_PIPE_ACCESS & 0x0001, 0, "read data");
        assert_ne!(INTERACTIVE_PIPE_ACCESS & 0x0002, 0, "write data");
        assert!(!PIPE_SDDL.contains("GRGW"));
        assert!(
            PIPE_SDDL.contains("(A;;GA;;;OW)"),
            "the creator needs Owner Rights to replace disconnected server instances"
        );
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

    #[test]
    fn ordinary_same_session_processes_are_not_trusted_as_the_ui() {
        assert!(
            !client_identity_evidence_allowed(true, false, false),
            "same-session portable paths are not process identity"
        );
        assert!(client_identity_evidence_allowed(true, true, false));
        assert!(client_identity_evidence_allowed(true, false, true));
        assert!(!client_identity_evidence_allowed(false, true, true));
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

        let client = open_test_pipe(name).expect("client connect");
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
        let client = open_test_pipe(name).expect("client connect");
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
                assert!(status
                    .capabilities
                    .iter()
                    .any(|x| x == eir_proto::CAP_COMMAND_RESULTS));
                // The startup snapshot a client receives on connect must advertise
                // everything this build supports: a UI that connects during startup
                // caches these and would otherwise refuse a supported retry.
                assert_eq!(status.capabilities, eir_proto::service_capabilities());
                assert!(status
                    .capabilities
                    .iter()
                    .any(|x| x == eir_proto::CAP_TARGETED_UPDATE_RETRY));
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

    #[tokio::test]
    async fn inactive_idle_session_releases_the_listener_for_the_next_client() {
        TEST_SESSION_ACTIVE.store(true, Ordering::Relaxed);
        let name: &'static str = Box::leak(
            format!(r"\\.\pipe\EirSvcTestSession-{}", std::process::id()).into_boxed_str(),
        );
        let (_srv, mut ui_rx) = spawn_named_with_verifier(
            name.to_string(),
            test_client_session,
            toggled_session_allowed,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _first = open_test_pipe(name).expect("first client");
        tokio::time::sleep(Duration::from_millis(150)).await;

        TEST_SESSION_ACTIVE.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        TEST_SESSION_ACTIVE.store(true, Ordering::Relaxed);

        let mut second = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match open_test_pipe(name) {
                    Ok(client) => break client,
                    Err(error) if matches!(error.raw_os_error(), Some(2) | Some(231)) => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) => panic!("second client failed: {error}"),
                }
            }
        })
        .await
        .expect("inactive session retained the only pipe listener");
        second
            .write_all(b"{\"type\":\"approve\",\"id\":9,\"approved\":false}\n")
            .await
            .expect("second client write");
        let request = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("second client request timeout")
            .expect("command channel");
        assert!(matches!(
            request.command,
            eir_proto::UiMsg::Approve {
                id: 9,
                approved: false
            }
        ));
        TEST_SESSION_ACTIVE.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn inactive_session_cancels_a_blocked_command_forward() {
        TEST_QUEUE_SESSION_ACTIVE.store(true, Ordering::Relaxed);
        let name: &'static str = Box::leak(
            format!(r"\\.\pipe\EirSvcTestQueuedSession-{}", std::process::id()).into_boxed_str(),
        );
        let (_srv, _full_rx) = spawn_named_with_verifier(
            name.to_string(),
            test_client_session,
            queued_session_allowed,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut first = open_test_pipe(name).expect("first client");
        tokio::time::sleep(Duration::from_millis(150)).await;

        let requests = (0..10)
            .map(|id| format!(r#"{{"type":"approve","id":{id},"approved":false}}"#))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        first
            .write_all(requests.as_bytes())
            .await
            .expect("fill command queue");
        tokio::time::sleep(Duration::from_millis(150)).await;

        TEST_QUEUE_SESSION_ACTIVE.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        TEST_QUEUE_SESSION_ACTIVE.store(true, Ordering::Relaxed);

        let second = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match open_test_pipe(name) {
                    Ok(client) => break client,
                    Err(error) if matches!(error.raw_os_error(), Some(2) | Some(231)) => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) => panic!("second client failed: {error}"),
                }
            }
        })
        .await;
        assert!(
            second.is_ok(),
            "a stale session blocked on the full command queue retained the listener"
        );
        TEST_QUEUE_SESSION_ACTIVE.store(true, Ordering::Relaxed);
    }
}
