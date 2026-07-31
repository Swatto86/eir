use anyhow::Result;
use eir_proto::{CommandResult, ServiceMsg, StatusPayload, UiRequest, PIPE_NAME};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::windows::{ffi::OsStringExt, fs::MetadataExt, io::AsRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::NamedPipeClient,
    sync::{mpsc, oneshot},
};
use tracing::{info, warn};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            EqualSid, GetTokenInformation, TokenElevationType, TokenElevationTypeDefault,
            TokenElevationTypeLimited, TokenSessionId, TokenUser, TOKEN_ELEVATION_TYPE,
            TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            CreateFileW, GetDriveTypeW, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_SHARE_MODE,
            FILE_WRITE_DATA, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        },
        System::Pipes::GetNamedPipeServerProcessId,
        System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
            PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};
use windows_service::{
    service::{ServiceAccess, ServiceState, ServiceType},
    service_manager::{ServiceManager, ServiceManagerAccess},
};

type ServiceVerifier = fn(u32) -> bool;

/// Largest service-to-UI JSON line accepted before dropping the connection.
const MAX_SERVICE_LINE_BYTES: u64 = 12 * 1024 * 1024;

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

fn regular_without_reparse(path: &Path) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    })
}

fn portable_mode_requested_at(flag: Option<&OsStr>, executable: &Path) -> bool {
    flag == Some(OsStr::new("1"))
        && executable
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("eir.exe"))
        && regular_without_reparse(executable)
        && executable
            .parent()
            .is_some_and(|parent| regular_without_reparse(&parent.join("eir-portable.running")))
}

fn portable_pipe_name() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    if !portable_mode_requested_at(std::env::var_os("EIR_PORTABLE").as_deref(), &executable) {
        return None;
    }
    validated_portable_pipe_name(std::env::var("EIR_PORTABLE_PIPE").ok()?)
}

fn validated_portable_pipe_name(pipe_name: String) -> Option<String> {
    let nonce = pipe_name.strip_prefix(r"\\.\pipe\EirSvcPortable-")?;
    (nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(pipe_name)
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
    if let Some(pipe_name) = portable_pipe_name() {
        info!("Using private portable service pipe");
        run_on_with_verifier(
            status,
            cmd_rx,
            &pipe_name,
            connected,
            waiters,
            verified_portable_service_process,
        )
        .await;
    } else {
        info!("Using installed service pipe");
        run_on(status, cmd_rx, PIPE_NAME, connected, waiters).await;
    }
}

async fn run_on(
    status: SharedStatus,
    cmd_rx: mpsc::Receiver<UiRequest>,
    pipe_name: &str,
    connected: Arc<AtomicBool>,
    waiters: CommandWaiters,
) {
    run_on_with_verifier(
        status,
        cmd_rx,
        pipe_name,
        connected,
        waiters,
        verified_service_process,
    )
    .await;
}

async fn run_on_with_verifier(
    status: SharedStatus,
    mut cmd_rx: mpsc::Receiver<UiRequest>,
    pipe_name: &str,
    connected: Arc<AtomicBool>,
    waiters: CommandWaiters,
    verify_server: ServiceVerifier,
) {
    loop {
        let result = connect_and_run(
            &status,
            &mut cmd_rx,
            pipe_name,
            &connected,
            &waiters,
            verify_server,
        )
        .await;
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
            s.svc_version = None;
            // Settings belong to this service snapshot too. Leaving them populated
            // keeps the frontend's one-shot hydration latch set, so a restarted
            // service can reconnect with different settings that are never rendered.
            s.settings = None;
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
    verify_server: ServiceVerifier,
) -> Result<()> {
    // Keep trying until the pipe is available (service may still be starting).
    let client = loop {
        match open_pipe(pipe_name) {
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

    let server_process_id = pipe_server_process_id(&client)
        .ok_or_else(|| anyhow::anyhow!("Cannot identify the service pipe server"))?;
    if !verify_service_off_runtime(verify_server, server_process_id).await {
        return Err(anyhow::anyhow!(
            "Refused untrusted service pipe server process {server_process_id}"
        ));
    }
    // Re-read the pipe's recorded owner after the SCM check to detect an ordinary
    // server change while authentication was in flight.
    if pipe_server_process_id(&client) != Some(server_process_id) {
        return Err(anyhow::anyhow!(
            "Service pipe server changed during identity verification"
        ));
    }

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
        let mut line = Vec::new();
        loop {
            line.clear();
            let mut limited = (&mut reader).take(MAX_SERVICE_LINE_BYTES);
            match limited.read_until(b'\n', &mut line).await {
                Ok(0) => break, // service disconnected
                Ok(_) if !line.ends_with(b"\n") => {
                    warn!(
                        "Service message exceeded {MAX_SERVICE_LINE_BYTES} bytes — dropping connection"
                    );
                    break;
                }
                Ok(_) => {
                    if line.iter().any(|byte| !byte.is_ascii_whitespace()) {
                        match serde_json::from_slice::<ServiceMsg>(&line) {
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
            if let Err(e) = writer.flush().await {
                warn!("Pipe flush error: {e}");
                break;
            }
        }
    };

    tokio::select! {
        _ = read_loop => {}
        _ = write_loop => {}
    }
    Ok(())
}

fn open_pipe(pipe_name: &str) -> std::io::Result<NamedPipeClient> {
    let path: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            // GENERIC_WRITE includes FILE_CREATE_PIPE_INSTANCE for named pipes.
            FILE_READ_DATA.0 | FILE_WRITE_DATA.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT,
            None,
        )
    }
    .map_err(|error| std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xFFFF) as i32))?;
    unsafe { NamedPipeClient::from_raw_handle(handle.0) }
}

async fn verify_service_off_runtime(verifier: ServiceVerifier, process_id: u32) -> bool {
    tokio::task::spawn_blocking(move || verifier(process_id))
        .await
        .unwrap_or(false)
}

fn pipe_server_process_id(
    client: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> Option<u32> {
    let mut process_id = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(client.as_raw_handle()), &mut process_id) }.ok()?;
    (process_id != 0).then_some(process_id)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn process_token(process: HANDLE) -> Option<OwnedHandle> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    Some(OwnedHandle(token))
}

fn token_session_id(token: HANDLE) -> Option<u32> {
    let mut session = u32::MAX;
    let mut returned = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            Some((&mut session as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        )
    }
    .ok()?;
    (returned as usize >= std::mem::size_of::<u32>() && session != u32::MAX).then_some(session)
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

fn portable_service_token_allowed(elevation_type: Option<TOKEN_ELEVATION_TYPE>) -> bool {
    matches!(
        elevation_type,
        Some(value)
            if value == TokenElevationTypeDefault || value == TokenElevationTypeLimited
    )
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

fn tokens_share_user(left: HANDLE, right: HANDLE) -> bool {
    let Some(left_user) = token_user_buffer(left) else {
        return false;
    };
    let Some(right_user) = token_user_buffer(right) else {
        return false;
    };
    let left_user = unsafe { &*left_user.as_ptr().cast::<TOKEN_USER>() };
    let right_user = unsafe { &*right_user.as_ptr().cast::<TOKEN_USER>() };
    unsafe { EqualSid(left_user.User.Sid, right_user.User.Sid) }.is_ok()
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

fn windows_path_eq(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn verified_portable_service_process(process_id: u32) -> bool {
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
    {
        Ok(process) => OwnedHandle(process),
        Err(_) => return false,
    };
    let Some(server_token) = process_token(process.0) else {
        return false;
    };
    let Some(ui_token) = process_token(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    if !portable_service_token_allowed(token_elevation_type(server_token.0))
        || token_session_id(server_token.0) != token_session_id(ui_token.0)
        || !tokens_share_user(server_token.0, ui_token.0)
    {
        return false;
    }
    let Some(image) = process_image_path(process.0)
        .filter(|path| trusted_service_image_path(path))
        .and_then(|path| std::fs::canonicalize(path).ok())
    else {
        return false;
    };
    let Some(expected) = std::env::current_exe()
        .ok()
        .and_then(|ui| ui.parent().map(|parent| parent.join("eir-svc.exe")))
        .filter(|path| trusted_service_image_path(path))
        .and_then(|path| std::fs::canonicalize(path).ok())
    else {
        return false;
    };
    windows_path_eq(&image, &expected)
}

fn verified_service_process(process_id: u32) -> bool {
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(manager) => manager,
        Err(_) => return false,
    };
    let service = match manager.open_service(
        "EirSvc",
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(service) => service,
        Err(_) => return false,
    };
    let status = match service.query_status() {
        Ok(status) => status,
        Err(_) => return false,
    };
    if status.current_state != ServiceState::Running || status.process_id != Some(process_id) {
        return false;
    }
    let config = match service.query_config() {
        Ok(config) => config,
        Err(_) => return false,
    };
    if config.service_type != ServiceType::OWN_PROCESS
        || !local_system_account(config.account_name.as_deref())
    {
        return false;
    }
    let Some(image) = service_executable_path(&config.executable_path) else {
        return false;
    };
    if !trusted_service_image_path(&image) {
        return false;
    }
    service.query_status().is_ok_and(|status| {
        status.current_state == ServiceState::Running && status.process_id == Some(process_id)
    })
}

fn local_system_account(account: Option<&OsStr>) -> bool {
    account.is_some_and(|account| {
        let account = account.to_string_lossy();
        ["LocalSystem", r".\LocalSystem", r"NT AUTHORITY\SYSTEM"]
            .iter()
            .any(|expected| account.eq_ignore_ascii_case(expected))
    })
}

fn service_executable_path(configured: &Path) -> Option<PathBuf> {
    let configured = configured.as_os_str().to_string_lossy();
    if let Some(quoted) = configured
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return (!quoted.contains('"')).then(|| PathBuf::from(quoted));
    }
    (!configured.chars().any(char::is_whitespace)).then(|| PathBuf::from(configured.as_ref()))
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

fn trusted_service_image_path(path: &Path) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let expected_name = path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("eir-svc.exe"));
    let Some(root) = fixed_drive_root(path) else {
        return false;
    };
    if !expected_name || unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != 3 {
        return false;
    }

    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::Disk(_)) => {
                current.push(component.as_os_str());
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(_) => {
                current.push(component.as_os_str());
                let Ok(metadata) = std::fs::symlink_metadata(&current) else {
                    return false;
                };
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return false;
                }
            }
            Component::CurDir => {}
            _ => return false,
        }
    }
    current.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

    #[tokio::test]
    async fn untrusted_server_is_not_marked_connected() {
        let name = format!(r"\\.\pipe\EirSvcTestUntrusted-{}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .pipe_mode(PipeMode::Byte)
            .create(&name)
            .expect("create fake server");
        let status: SharedStatus = Arc::new(Mutex::new(StatusPayload::default()));
        let (_tx, rx) = mpsc::channel::<UiRequest>(1);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_for_client = connected.clone();
        let waiters: CommandWaiters = Arc::new(Mutex::new(HashMap::new()));
        let status_for_client = status.clone();
        let name_for_client = name.clone();

        tokio::spawn(async move {
            run_on(
                status_for_client,
                rx,
                &name_for_client,
                connected_for_client,
                waiters,
            )
            .await
        });
        server.connect().await.expect("fake server accepts client");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock_status(&status).status != "ServiceDisconnected" {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("client should finish authenticating and reject the fake server");

        assert!(
            !connected.load(Ordering::Relaxed),
            "an arbitrary same-user pipe server must not become the trusted service"
        );
    }

    #[test]
    fn service_registration_accepts_only_local_system_and_one_executable() {
        assert!(local_system_account(Some(OsStr::new("LocalSystem"))));
        assert!(local_system_account(Some(OsStr::new(
            r"NT AUTHORITY\SYSTEM"
        ))));
        assert!(!local_system_account(Some(OsStr::new("LocalService"))));
        assert!(!local_system_account(None));

        assert_eq!(
            service_executable_path(Path::new(r#""C:\Program Files\Eir\eir-svc.exe""#)),
            Some(PathBuf::from(r"C:\Program Files\Eir\eir-svc.exe"))
        );
        assert_eq!(
            service_executable_path(Path::new(r"C:\Eir\eir-svc.exe")),
            Some(PathBuf::from(r"C:\Eir\eir-svc.exe"))
        );
        assert_eq!(
            service_executable_path(Path::new(r"C:\Program Files\Eir\eir-svc.exe --arg")),
            None
        );
    }

    #[test]
    fn portable_mode_requires_launcher_flag_and_regular_sibling_sentinel() {
        let root =
            std::env::temp_dir().join(format!("eir-ui-portable-mode-{}", std::process::id()));
        let ui = root.join("eir.exe");
        let sentinel = root.join("eir-portable.running");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create portable test directory");
        std::fs::write(&ui, b"test").expect("write test UI");
        std::fs::write(&sentinel, b"running").expect("write test sentinel");

        assert!(!portable_mode_requested_at(None, &ui));
        assert!(portable_mode_requested_at(Some(OsStr::new("1")), &ui));
        std::fs::remove_file(&sentinel).expect("remove sentinel");
        assert!(!portable_mode_requested_at(Some(OsStr::new("1")), &ui));

        std::fs::remove_dir_all(root).expect("remove portable test directory");
    }

    #[test]
    fn portable_service_token_policy_rejects_split_elevation() {
        assert!(portable_service_token_allowed(Some(
            TokenElevationTypeDefault
        )));
        assert!(portable_service_token_allowed(Some(
            TokenElevationTypeLimited
        )));
        assert!(!portable_service_token_allowed(Some(
            windows::Win32::Security::TokenElevationTypeFull
        )));
        assert!(!portable_service_token_allowed(None));
    }

    #[test]
    fn portable_pipe_name_is_private_and_bounded() {
        assert!(validated_portable_pipe_name(
            r"\\.\pipe\EirSvcPortable-0123456789abcdef0123456789abcdef".to_string()
        )
        .is_some());
        assert!(validated_portable_pipe_name(PIPE_NAME.to_string()).is_none());
        assert!(validated_portable_pipe_name(
            r"\\.\pipe\EirSvcPortable-0123456789abcdef0123456789abcde!".to_string()
        )
        .is_none());
    }

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
        tokio::spawn(async move {
            run_on_with_verifier(status_c, rx, &name_owned, connected_c, waiters, |_| true).await
        });

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
        lock_status(&status).svc_version = Some("stale-service-version".to_string());
        lock_status(&status).settings = Some(eir_proto::UiSettings::default());
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
        assert!(
            lock_status(&status).svc_version.is_none(),
            "the disconnected service version must not be shown as current"
        );
        assert!(
            lock_status(&status).settings.is_none(),
            "settings from the disconnected service must not suppress hydration from the next service"
        );
    }

    #[tokio::test]
    async fn oversized_service_line_disconnects_the_client() {
        let name = format!(r"\\.\pipe\EirSvcTestOversize-{}", std::process::id());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .pipe_mode(PipeMode::Byte)
            .create(&name)
            .expect("create server");
        let status: SharedStatus = Arc::new(Mutex::new(StatusPayload::default()));
        let (_tx, rx) = mpsc::channel::<UiRequest>(1);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_for_client = connected.clone();
        let waiters: CommandWaiters = Arc::new(Mutex::new(HashMap::new()));
        let name_for_client = name.clone();

        tokio::spawn(async move {
            run_on_with_verifier(
                status,
                rx,
                &name_for_client,
                connected_for_client,
                waiters,
                |_| true,
            )
            .await
        });
        server.connect().await.expect("server accept client");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !connected.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("client connects");

        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..(MAX_SERVICE_LINE_BYTES as usize / chunk.len()) {
            server
                .write_all(&chunk)
                .await
                .expect("write oversized line");
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while connected.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("oversized line should drop the connection");
    }

    #[test]
    fn ensure_connected_gate() {
        let flag = AtomicBool::new(false);
        assert!(crate::ensure_connected(&flag).is_err());
        flag.store(true, Ordering::Relaxed);
        assert!(crate::ensure_connected(&flag).is_ok());
    }
}
