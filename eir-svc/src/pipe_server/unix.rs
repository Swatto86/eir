//! Linux control-plane server: a Unix domain socket (`service.socket_path`, default
//! `/run/eir/eir.sock`) instead of a Windows named pipe — the same `UiRequest`/
//! `ServiceMsg` wire protocol, no format change. Authorisation is `SO_PEERCRED`
//! (`UnixStream::peer_cred()`, stable in tokio's `net` feature — no new dependency),
//! checked once immediately after `accept()` and before a single byte is read: uid 0
//! (root/sudo — the direct analogue of the Windows pipe's "elevated Administrators...
//! accepted for maintenance/CI") or any uid/gid on `service.socket_allow_uids`/
//! `socket_allow_gids` is allowed; everything else is dropped with zero bytes read.
//! Unlike the Windows pipe (one client at a time), several `eirctl` connections can be
//! open concurrently — the wire protocol's `request_id` correlation already makes that
//! safe (each client's write task only pushes `CommandResult`s the whole broadcast
//! channel carries, and every client filters by its own `request_id`).
//!
//! Filesystem permissions on `/run/eir` and the socket itself are deliberately
//! permissive (directory `0755`, socket `0666`) — `peer_cred()` above is the real,
//! authoritative gate, exactly like the Windows pipe already documents for its own DACL
//! ("elevated Administrators... accepted for maintenance/CI"). eir-svc runs as root
//! under systemd (`packaging/systemd/eir.service`'s `User=root`/`Group=root`, with no
//! dedicated group), so a tighter directory/socket mode (e.g. `0750`/`0660` root:root)
//! would make the documented `socket_allow_uids`/`socket_allow_gids` multi-user feature
//! unreachable for any uid outside root's own group: such a uid would get `EACCES`
//! opening the socket before a single byte — and therefore before `peer_cred()` — was
//! ever consulted, defeating the one interface (`eirctl`, run by the configured
//! non-root operator) the allowlist exists to admit. A non-allowed uid can still open()
//! and connect(); it is then rejected by `peer_cred()` and dropped with zero bytes read,
//! same as today.

use super::{PipeServer, ResultEnvelope};
use eir_proto::{StatusPayload, UiRequest};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

const DEFAULT_SOCKET_PATH: &str = "/run/eir/eir.sock";

fn configured_socket() -> (PathBuf, Vec<u32>, Vec<u32>) {
    match crate::config::load("config.toml") {
        Ok(cfg) => {
            let path = if cfg.service.socket_path.trim().is_empty() {
                DEFAULT_SOCKET_PATH.to_string()
            } else {
                cfg.service.socket_path.clone()
            };
            (
                PathBuf::from(path),
                cfg.service.socket_allow_uids.clone(),
                cfg.service.socket_allow_gids.clone(),
            )
        }
        Err(error) => {
            warn!(
                "Could not read config.toml for the control socket ({error}); using the \
                 default path with no extra allowed uids/gids"
            );
            (PathBuf::from(DEFAULT_SOCKET_PATH), Vec::new(), Vec::new())
        }
    }
}

pub fn spawn() -> (PipeServer, mpsc::Receiver<UiRequest>) {
    let (path, allow_uids, allow_gids) = configured_socket();
    let (srv, ui_cmd_rx, status_tx, result_tx, ui_cmd_tx, clients_tx) = super::new_server();

    tokio::spawn(listener_task(
        path, allow_uids, allow_gids, status_tx, result_tx, ui_cmd_tx, clients_tx,
    ));

    (srv, ui_cmd_rx)
}

/// Headless Linux has no portable-mode analogue; kept only so `main.rs`'s shared
/// `eir_main(shutdown, portable_pipe)` match arms compile unchanged on both platforms.
/// This arm is never actually reached: the `#[cfg(unix)]` entry point always calls
/// `eir_main` with `portable_pipe: None`.
pub fn spawn_portable(_pipe_name: String) -> (PipeServer, mpsc::Receiver<UiRequest>) {
    spawn()
}

/// Bind the control socket, handling a stale socket left by an uncleanly-terminated
/// previous run: a successful `connect()` means a live server already owns it (fail
/// loudly, never steal it); `ConnectionRefused`/`NotFound` means the previous process
/// died without cleaning up — remove the stale file and bind fresh. Any other connect
/// error is surfaced as-is rather than risking removing a socket for an unknown reason.
async fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
        // 0755 (not 0750): the directory only needs to be traversable so any
        // configured uid/gid can reach the socket — see the module doc comment on why
        // `peer_cred()`, not directory/socket DAC bits, is the real gate here.
        let _ = tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).await;
    }
    match UnixStream::connect(path).await {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("a live eir-svc already owns {}", path.display()),
            ));
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            let _ = tokio::fs::remove_file(path).await;
        }
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    // bind()'s own mode is subject to umask, so set it deterministically afterward.
    // 0666 (not 0660): connecting still requires passing peer_cred() below, so the
    // socket's own DAC bits are not the security boundary — see the module doc comment.
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).await?;
    Ok(listener)
}

fn peer_allowed(uid: u32, gid: u32, allow_uids: &[u32], allow_gids: &[u32]) -> bool {
    uid == 0 || allow_uids.contains(&uid) || allow_gids.contains(&gid)
}

async fn listener_task(
    path: PathBuf,
    allow_uids: Vec<u32>,
    allow_gids: Vec<u32>,
    status_tx: watch::Sender<StatusPayload>,
    result_tx: broadcast::Sender<ResultEnvelope>,
    ui_cmd_tx: mpsc::Sender<UiRequest>,
    clients: watch::Sender<usize>,
) {
    let listener = loop {
        match bind_socket(&path).await {
            Ok(listener) => break listener,
            Err(error) => {
                warn!("Could not bind control socket {}: {error}", path.display());
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };
    info!("Control socket listening on {}", path.display());

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                warn!("Control socket accept error: {error}");
                continue;
            }
        };
        let Ok(cred) = stream.peer_cred() else {
            warn!("Could not read peer credentials for a control-socket connection — rejecting");
            continue;
        };
        if !peer_allowed(cred.uid(), cred.gid(), &allow_uids, &allow_gids) {
            warn!(
                uid = cred.uid(),
                gid = cred.gid(),
                "Rejected control-socket client outside the configured allowlist"
            );
            continue; // dropped with zero bytes read
        }

        let status_tx = status_tx.clone();
        let result_tx = result_tx.clone();
        let ui_cmd_tx = ui_cmd_tx.clone();
        let clients = clients.clone();
        // Peer-cred is checked once at accept, not on every subsequent read — a
        // narrow, low-value attack given the fixed root/allowlisted-gid trust model
        // (a Unix uid/gid cannot change under a live connection, unlike a Windows
        // desktop session). A liveness watch that never fires away is therefore
        // correct; it must stay alive for the connection's duration, or
        // `handle_connection` would see the channel close and end the connection
        // immediately.
        let (keep_alive, session_rx) = watch::channel(true);
        tokio::spawn(async move {
            let _keep_alive = keep_alive;
            super::handle_connection(stream, status_tx, result_tx, ui_cmd_tx, clients, session_rx)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eir_proto::{CommandResult, ServiceMsg, UiMsg};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn root_is_always_allowed() {
        assert!(peer_allowed(0, 1000, &[], &[]));
    }

    #[test]
    fn a_configured_uid_or_gid_is_allowed() {
        assert!(peer_allowed(1000, 1000, &[1000], &[]));
        assert!(peer_allowed(1000, 999, &[], &[999]));
    }

    #[test]
    fn an_unlisted_peer_is_rejected() {
        assert!(!peer_allowed(1001, 1001, &[1000], &[999]));
    }

    fn test_socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "eir-pipe-server-test-{name}-{}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_not_left_blocking_a_fresh_bind() {
        let path = test_socket_path("stale");
        let _ = std::fs::remove_file(&path);
        // A socket file with nothing listening behind it (simulates an unclean exit):
        // bind once, then drop the listener without unlinking the path.
        {
            let listener = UnixListener::bind(&path).expect("first bind");
            drop(listener);
        }
        assert!(path.exists(), "the stale socket file must still be present");
        let rebound = bind_socket(&path).await;
        assert!(
            rebound.is_ok(),
            "a stale socket must be replaced: {rebound:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_live_socket_is_never_stolen() {
        let path = test_socket_path("live");
        let _ = std::fs::remove_file(&path);
        let _listener = UnixListener::bind(&path).expect("bind live socket");
        let result = bind_socket(&path).await;
        assert!(
            result.is_err(),
            "a socket a live listener still owns must never be rebound out from under it"
        );
        let _ = std::fs::remove_file(&path);
    }

    async fn spawn_test_server(
        allow_uids: Vec<u32>,
    ) -> (PathBuf, PipeServer, mpsc::Receiver<UiRequest>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = test_socket_path(&format!("srv-{n}"));
        let _ = std::fs::remove_file(&path);
        let (srv, ui_cmd_rx, status_tx, result_tx, ui_cmd_tx, clients_tx) =
            super::super::new_server();
        tokio::spawn(listener_task(
            path.clone(),
            allow_uids,
            Vec::new(),
            status_tx,
            result_tx,
            ui_cmd_tx,
            clients_tx,
        ));
        // Give the listener a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        (path, srv, ui_cmd_rx)
    }

    #[tokio::test]
    async fn a_non_allowlisted_uid_is_refused_before_a_single_byte_is_read() {
        // This test process's own uid is real but deliberately left off the allowlist
        // (which names a uid that cannot be this test's own, 0, being root — the test
        // harness never runs as root) — proving the connection is dropped rather than
        // silently accepted for every caller.
        let (path, _srv, _ui_rx) = spawn_test_server(vec![999_999]).await;
        let mut client = UnixStream::connect(&path).await.expect("connect");
        let mut buf = [0u8; 1];
        use tokio::io::AsyncReadExt;
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("read should not hang")
            .expect("read completes (0 = EOF)");
        assert_eq!(read, 0, "a rejected client must see EOF, not a Status line");
        let _ = std::fs::remove_file(&path);
    }

    /// Read `ServiceMsg` lines until finding a `CommandResult` whose `request_id`
    /// matches — the same client-side filter `eirctl` applies (the server broadcasts
    /// every result to every connected client; a client picks its own out by id, see
    /// this module's doc comment and `eir-cli/src/unix.rs::wait_for_result`).
    async fn read_own_result<R: tokio::io::AsyncBufRead + Unpin>(
        reader: &mut R,
        request_id: u64,
    ) -> CommandResult {
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("read a service line");
            match serde_json::from_str::<ServiceMsg>(line.trim()).expect("service line decodes") {
                ServiceMsg::CommandResult(result) if result.request_id == request_id => {
                    return result
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn two_simultaneous_clients_get_independent_correlated_results_with_no_cross_talk() {
        let uid = unsafe { libc::getuid() };
        let (path, srv, mut ui_rx) = spawn_test_server(vec![uid]).await;

        let mut client_a = UnixStream::connect(&path).await.expect("client a connect");
        let mut client_b = UnixStream::connect(&path).await.expect("client b connect");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let (mut ra, mut wa) = client_a.split();
        let mut ra = BufReader::new(&mut ra);
        let (mut rb, mut wb) = client_b.split();
        let mut rb = BufReader::new(&mut rb);

        // Both get an initial Status line on connect.
        let mut line = String::new();
        ra.read_line(&mut line).await.expect("a initial status");
        line.clear();
        rb.read_line(&mut line).await.expect("b initial status");

        wa.write_all(br#"{"type":"approve","id":1,"approved":true,"request_id":11}"#)
            .await
            .expect("a write");
        wa.write_all(b"\n").await.expect("a newline");
        wb.write_all(br#"{"type":"approve","id":2,"approved":true,"request_id":22}"#)
            .await
            .expect("b write");
        wb.write_all(b"\n").await.expect("b newline");

        let req_a = ui_rx.recv().await.expect("request a");
        let req_b = ui_rx.recv().await.expect("request b");
        assert_eq!(req_a.request_id, Some(11));
        assert_eq!(req_b.request_id, Some(22));
        assert!(matches!(req_a.command, UiMsg::Approve { id: 1, .. }));
        assert!(matches!(req_b.command, UiMsg::Approve { id: 2, .. }));

        // The server broadcasts every CommandResult to every connected client (proven
        // by both clients reading past the other's result below) — each client's own
        // `request_id` filter is what makes the multi-client model safe, not the
        // server withholding anything. Sent deliberately out of "b, then a" order so
        // a client that naively took the first result it saw would get the wrong one.
        srv.command_result(Some(22), Ok("b done".to_string()));
        srv.command_result(Some(11), Ok("a done".to_string()));

        let result_a = read_own_result(&mut ra, 11).await;
        assert_eq!(result_a.message, "a done");
        let result_b = read_own_result(&mut rb, 22).await;
        assert_eq!(result_b.message, "b done");

        let _ = std::fs::remove_file(&path);
    }
}
