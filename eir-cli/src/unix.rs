//! The real `eirctl` client: connect, send one `UiMsg`, correlate the reply by
//! `request_id`, print, exit. Hand-rolled argument dispatch — seven subcommands and one
//! flag do not justify a `clap` dependency.

use eir_proto::{CommandResult, ServiceMsg, StatusPayload, UiMsg, UiRequest};
use std::env;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::{timeout, Instant};

const DEFAULT_SOCKET: &str = "/run/eir/eir.sock";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_ASK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const STILL_WAITING_STEP: Duration = Duration::from_secs(15);

pub(crate) fn run() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("eirctl: could not start async runtime: {error}");
            return ExitCode::from(3);
        }
    };
    runtime.block_on(async_main())
}

struct Args {
    json: bool,
    timeout: Option<Duration>,
    positional: Vec<String>,
}

fn parse_args() -> Args {
    let mut json = false;
    let mut timeout_secs: Option<u64> = None;
    let mut positional = Vec::new();
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--timeout" => timeout_secs = iter.next().and_then(|value| value.parse().ok()),
            other => positional.push(other.to_string()),
        }
    }
    Args {
        json,
        timeout: timeout_secs.map(Duration::from_secs),
        positional,
    }
}

fn print_usage() {
    eprintln!(
        "Usage: eirctl [--json] <command> [args] [--timeout <secs>]\n\
         Commands:\n  \
           status                    print live service status\n  \
           approvals                 list actions awaiting approval\n  \
           approve <id>               approve a pending action\n  \
           reject <id>                reject a pending action\n  \
           pause                     pause monitoring (no-op if already paused)\n  \
           resume                    resume monitoring (no-op if already running)\n  \
           ask \"<question>\"           ask Eir a question\n  \
           investigate \"<description>\" investigate and fix a described problem"
    );
}

async fn async_main() -> ExitCode {
    let args = parse_args();
    let Some(verb) = args.positional.first().cloned() else {
        print_usage();
        return ExitCode::from(2);
    };
    match verb.as_str() {
        "status" => cmd_status(args.json).await,
        "approvals" => cmd_approvals(args.json).await,
        "approve" => cmd_approve_or_reject(args.positional.get(1), true).await,
        "reject" => cmd_approve_or_reject(args.positional.get(1), false).await,
        "pause" => cmd_set_paused(true).await,
        "resume" => cmd_set_paused(false).await,
        "ask" => cmd_ask_or_investigate(&args, false).await,
        "investigate" => cmd_ask_or_investigate(&args, true).await,
        "help" | "-h" | "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        _ => {
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn socket_path() -> String {
    env::var("EIR_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

async fn connect() -> Result<Connection, ExitCode> {
    let path = socket_path();
    match timeout(CONNECT_TIMEOUT, UnixStream::connect(&path)).await {
        Ok(Ok(stream)) => {
            let (read_half, write_half) = stream.into_split();
            Ok(Connection {
                reader: BufReader::new(read_half),
                writer: write_half,
            })
        }
        Ok(Err(error)) => {
            eprintln!("eirctl: could not connect to {path}: {error}");
            Err(ExitCode::from(3))
        }
        Err(_) => {
            eprintln!("eirctl: timed out connecting to {path}");
            Err(ExitCode::from(3))
        }
    }
}

async fn read_message(reader: &mut BufReader<OwnedReadHalf>) -> Option<ServiceMsg> {
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => None,
        Ok(_) => serde_json::from_str(line.trim()).ok(),
        Err(_) => None,
    }
}

/// The server always pushes an unsolicited `Status` line immediately on connect.
/// Commands that don't need it still read (and discard) it, so the stream stays in
/// sync for whatever is read next.
async fn initial_status(reader: &mut BufReader<OwnedReadHalf>) -> Option<StatusPayload> {
    match read_message(reader).await {
        Some(ServiceMsg::Status(status)) => Some(*status),
        _ => None,
    }
}

/// A one-shot process needs no shared counter: `request_id` is a caller-chosen
/// correlation echo the service repeats back verbatim.
fn next_request_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ u64::from(std::process::id())
}

async fn wait_for_result(
    reader: &mut BufReader<OwnedReadHalf>,
    request_id: u64,
) -> Option<CommandResult> {
    loop {
        match read_message(reader).await {
            Some(ServiceMsg::CommandResult(result)) if result.request_id == request_id => {
                return Some(result)
            }
            Some(_) => continue,
            None => return None,
        }
    }
}

async fn send_request(conn: &mut Connection, request: &UiRequest) -> Result<(), ExitCode> {
    let mut line = match serde_json::to_string(request) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("eirctl: could not encode request: {error}");
            return Err(ExitCode::from(3));
        }
    };
    line.push('\n');
    if let Err(error) = conn.writer.write_all(line.as_bytes()).await {
        eprintln!("eirctl: could not send command: {error}");
        return Err(ExitCode::from(3));
    }
    if let Err(error) = conn.writer.flush().await {
        eprintln!("eirctl: could not send command: {error}");
        return Err(ExitCode::from(3));
    }
    Ok(())
}

/// Send `command`, wait for its correlated `CommandResult`, print the message and map
/// `ok`/timeout/disconnect to the documented exit codes (0 success, 1 rejected, 3
/// connect/timeout failure — a usage error never reaches this far).
async fn send_and_report(conn: &mut Connection, command: UiMsg) -> ExitCode {
    let request_id = next_request_id();
    let request = UiRequest {
        request_id: Some(request_id),
        command,
    };
    if let Err(code) = send_request(conn, &request).await {
        return code;
    }
    match timeout(
        COMMAND_TIMEOUT,
        wait_for_result(&mut conn.reader, request_id),
    )
    .await
    {
        Ok(Some(result)) if result.ok => {
            println!("{}", result.message);
            ExitCode::SUCCESS
        }
        Ok(Some(result)) => {
            eprintln!("{}", result.message);
            ExitCode::from(1)
        }
        Ok(None) => {
            eprintln!("eirctl: the service closed the connection before replying");
            ExitCode::from(3)
        }
        Err(_) => {
            eprintln!("eirctl: timed out waiting for a reply");
            ExitCode::from(3)
        }
    }
}

async fn cmd_status(json: bool) -> ExitCode {
    let mut conn = match connect().await {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    match initial_status(&mut conn.reader).await {
        Some(status) => {
            print_status(&status, json);
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("eirctl: the service closed the connection before sending status");
            ExitCode::from(3)
        }
    }
}

fn print_status(status: &StatusPayload, json: bool) {
    if json {
        match serde_json::to_string_pretty(status) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("eirctl: could not encode status: {error}"),
        }
        return;
    }
    println!("status:             {}", status.status);
    println!("paused:             {}", status.paused);
    if let Some(error) = &status.error {
        println!("error:              {error}");
    }
    println!(
        "cpu / mem / disk:   {:.1}% / {:.1}% / {:.1}%",
        status.cpu, status.memory, status.disk
    );
    if status.failed_services.is_empty() {
        println!("failed services:    none");
    } else {
        println!("failed services:    {}", status.failed_services.join(", "));
    }
    if status.last_analysis_at > 0 {
        println!(
            "last analysis:      {} (unix seconds)",
            status.last_analysis_at
        );
    } else {
        println!("last analysis:      never");
    }
    println!("pending approvals:  {}", status.pending_approvals.len());
    if !status.recent_signals.is_empty() {
        println!("recent signals:");
        for signal in status.recent_signals.iter().take(10) {
            println!("  [{}] {} — {}", signal.source, signal.app, signal.summary);
        }
    }
    if !status.recent_executions.is_empty() {
        println!("recent fixes:");
        for execution in status.recent_executions.iter().take(10) {
            let verdict = if execution.success { "ok" } else { "failed" };
            println!("  {} — {verdict}", execution.action);
        }
    }
}

async fn cmd_approvals(json: bool) -> ExitCode {
    let mut conn = match connect().await {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    let Some(status) = initial_status(&mut conn.reader).await else {
        eprintln!("eirctl: the service closed the connection before sending status");
        return ExitCode::from(3);
    };
    if json {
        match serde_json::to_string_pretty(&status.pending_approvals) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("eirctl: could not encode approvals: {error}");
                return ExitCode::from(3);
            }
        }
    } else if status.pending_approvals.is_empty() {
        println!("No pending approvals.");
    } else {
        for approval in &status.pending_approvals {
            println!(
                "#{id}  {diagnosis}\n    target: {target}   confidence: {confidence:.0}%   reversible: {reversible}\n    {summary}\n",
                id = approval.id,
                diagnosis = approval.diagnosis,
                target = approval.target,
                confidence = approval.confidence * 100.0,
                reversible = approval.reversible,
                summary = approval.action_summary,
            );
        }
    }
    ExitCode::SUCCESS
}

async fn cmd_approve_or_reject(id_arg: Option<&String>, approved: bool) -> ExitCode {
    let verb = if approved { "approve" } else { "reject" };
    let Some(id_str) = id_arg else {
        eprintln!("eirctl: {verb} requires an approval id");
        return ExitCode::from(2);
    };
    let Ok(id) = id_str.parse::<u64>() else {
        eprintln!("eirctl: '{id_str}' is not a valid approval id");
        return ExitCode::from(2);
    };
    let mut conn = match connect().await {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    let _ = initial_status(&mut conn.reader).await;
    send_and_report(&mut conn, UiMsg::Approve { id, approved }).await
}

async fn cmd_set_paused(pause: bool) -> ExitCode {
    let mut conn = match connect().await {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    let Some(status) = initial_status(&mut conn.reader).await else {
        eprintln!("eirctl: the service closed the connection before sending status");
        return ExitCode::from(3);
    };
    // Documented TOCTOU race with another client is accepted as an inherent
    // CLI-tool limitation: worst case, a concurrent toggle flips it back.
    if status.paused == pause {
        println!(
            "{}",
            if pause {
                "Already paused"
            } else {
                "Already running"
            }
        );
        return ExitCode::SUCCESS;
    }
    send_and_report(&mut conn, UiMsg::TogglePause).await
}

async fn cmd_ask_or_investigate(args: &Args, investigate: bool) -> ExitCode {
    let (label, arg) = if investigate {
        ("investigate", args.positional.get(1))
    } else {
        ("ask", args.positional.get(1))
    };
    let Some(text) = arg else {
        eprintln!("eirctl: {label} requires a question/description");
        return ExitCode::from(2);
    };
    let (command, answer_prefix) = if investigate {
        (
            UiMsg::Investigate {
                description: text.clone(),
            },
            Some("Investigate & fix: ".to_string()),
        )
    } else {
        (
            UiMsg::AskEir {
                question: text.clone(),
                attachments: Vec::new(),
            },
            None,
        )
    };

    let mut conn = match connect().await {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    let _ = initial_status(&mut conn.reader).await;
    let request_id = next_request_id();
    let request = UiRequest {
        request_id: Some(request_id),
        command,
    };
    if let Err(code) = send_request(&mut conn, &request).await {
        return code;
    }
    let accept = match timeout(
        COMMAND_TIMEOUT,
        wait_for_result(&mut conn.reader, request_id),
    )
    .await
    {
        Ok(Some(result)) => result,
        Ok(None) => {
            eprintln!("eirctl: the service closed the connection before replying");
            return ExitCode::from(3);
        }
        Err(_) => {
            eprintln!("eirctl: timed out waiting for a reply");
            return ExitCode::from(3);
        }
    };
    if !accept.ok {
        eprintln!("{}", accept.message);
        return ExitCode::from(1);
    }

    let sent_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let deadline = args.timeout.unwrap_or(DEFAULT_ASK_TIMEOUT);
    let started = Instant::now();
    let mut printed_waiting = false;
    loop {
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            eprintln!("eirctl: timed out waiting for an answer");
            return ExitCode::from(3);
        }
        let step = (deadline - elapsed).min(STILL_WAITING_STEP);
        match timeout(step, read_message(&mut conn.reader)).await {
            Ok(Some(ServiceMsg::Status(status))) => {
                let Some(ask) = &status.ask else { continue };
                if let Some(entry) = ask.entries.first() {
                    let matches = answer_prefix
                        .as_deref()
                        .is_none_or(|prefix| entry.question.starts_with(prefix));
                    if matches && !ask.running && entry.at >= sent_at {
                        println!("{}", entry.answer);
                        return ExitCode::SUCCESS;
                    }
                }
                if !ask.running {
                    if let Some(error) = &ask.error {
                        eprintln!("eirctl: {error}");
                        return ExitCode::from(1);
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                eprintln!("eirctl: the service closed the connection before answering");
                return ExitCode::from(3);
            }
            Err(_) => {
                if !printed_waiting {
                    eprintln!("eirctl: still waiting…");
                    printed_waiting = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eir_proto::{ApprovalInfo, AskEntry, AskStatus};
    use tokio::net::UnixListener;
    use tokio::sync::{Mutex, MutexGuard};

    /// `EIR_SOCKET` is process-global; cargo test runs tests in parallel by default,
    /// so every test below that sets it must hold this guard for its duration or a
    /// concurrently-running sibling can point `connect()` at the wrong fake server. A
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`) because the guard is held across
    /// `.await` points for the rest of each test.
    static EIR_SOCKET_ENV: Mutex<()> = Mutex::const_new(());

    /// Point `EIR_SOCKET` at `path` for the guard's lifetime, serialised against every
    /// other test that also touches it.
    async fn set_socket_env(path: &str) -> MutexGuard<'static, ()> {
        let guard = EIR_SOCKET_ENV.lock().await;
        std::env::set_var("EIR_SOCKET", path);
        guard
    }

    #[test]
    fn request_ids_from_consecutive_calls_differ() {
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, b, "two calls in quick succession must not collide");
    }

    /// Bind a fake in-process server on a fresh temp path and hand back the path plus
    /// the accepted connection's two halves once a client connects — the same shape
    /// `eirctl`'s own `connect()` produces, so these tests exercise the exact request
    /// framing / reply parsing / timeout logic the real binary uses.
    async fn fake_server() -> (String, UnixListener) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("eirctl-test-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind fake server");
        (path.to_string_lossy().into_owned(), listener)
    }

    fn write_line(status: &ServiceMsg) -> Vec<u8> {
        let mut line = serde_json::to_vec(status).expect("encode fixture message");
        line.push(b'\n');
        line
    }

    #[tokio::test]
    async fn initial_status_and_result_round_trip_through_the_real_wire_types() {
        let (path, listener) = fake_server().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);

            // Push the unsolicited Status line every real connection gets.
            let status = StatusPayload {
                status: "Active".to_string(),
                paused: false,
                pending_approvals: vec![ApprovalInfo {
                    id: 7,
                    diagnosis: "disk full".into(),
                    root_cause: String::new(),
                    confidence: 0.9,
                    action: String::new(),
                    reason: String::new(),
                    side_effects: String::new(),
                    undo_instructions: String::new(),
                    action_summary: "restart caddy.service".into(),
                    target: "caddy.service".into(),
                    target_details: String::new(),
                    reversible: true,
                    created_at: 0,
                }],
                ..Default::default()
            };
            write_half
                .write_all(&write_line(&ServiceMsg::Status(Box::new(status))))
                .await
                .expect("write initial status");

            // Read the client's UiRequest line, then answer it, correlated by id.
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read request");
            let request: UiRequest = serde_json::from_str(line.trim()).expect("decode request");
            assert!(matches!(
                request.command,
                UiMsg::Approve {
                    id: 7,
                    approved: true
                }
            ));
            let result = ServiceMsg::CommandResult(CommandResult {
                request_id: request.request_id.expect("request carries an id"),
                ok: true,
                message: "Unit 'caddy.service' restarted successfully".to_string(),
            });
            write_half
                .write_all(&write_line(&result))
                .await
                .expect("write result");
        });

        // SAFETY (test-only): eirctl reads EIR_SOCKET once per call in a
        // single-threaded runtime with no other thread touching env vars here.
        let _env = set_socket_env(&path).await;
        let mut conn = connect().await.expect("client connects to the fake server");
        let status = initial_status(&mut conn.reader)
            .await
            .expect("initial status decodes");
        assert_eq!(status.pending_approvals.len(), 1);
        assert_eq!(status.pending_approvals[0].id, 7);

        let request_id = 424_242;
        let request = UiRequest {
            request_id: Some(request_id),
            command: UiMsg::Approve {
                id: 7,
                approved: true,
            },
        };
        send_request(&mut conn, &request)
            .await
            .expect("send request");
        let result = timeout(
            Duration::from_secs(5),
            wait_for_result(&mut conn.reader, request_id),
        )
        .await
        .expect("must not time out")
        .expect("result decodes");
        assert!(result.ok);
        assert!(result.message.contains("restarted successfully"));

        server.await.expect("fake server task");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_reply_for_a_different_request_id_is_skipped_not_mistaken_for_this_ones() {
        let (path, listener) = fake_server().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (_read_half, mut write_half) = stream.into_split();
            write_half
                .write_all(&write_line(&ServiceMsg::Status(Box::default())))
                .await
                .expect("initial status");
            // A stale result for someone else's request first...
            write_half
                .write_all(&write_line(&ServiceMsg::CommandResult(CommandResult {
                    request_id: 111,
                    ok: false,
                    message: "not for you".to_string(),
                })))
                .await
                .expect("stale result");
            // ...then the real one.
            write_half
                .write_all(&write_line(&ServiceMsg::CommandResult(CommandResult {
                    request_id: 222,
                    ok: true,
                    message: "for you".to_string(),
                })))
                .await
                .expect("real result");
        });

        let _env = set_socket_env(&path).await;
        let mut conn = connect().await.expect("client connects");
        let _ = initial_status(&mut conn.reader).await;
        let result = timeout(
            Duration::from_secs(5),
            wait_for_result(&mut conn.reader, 222),
        )
        .await
        .expect("must not time out")
        .expect("result decodes");
        assert_eq!(result.message, "for you");

        server.await.expect("fake server task");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn connect_fails_closed_with_no_server_listening() {
        let path = std::env::temp_dir().join(format!(
            "eirctl-test-nobody-home-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _env = set_socket_env(&path.to_string_lossy()).await;
        assert!(
            connect().await.is_err(),
            "connecting to a socket with nothing listening must fail, not hang"
        );
    }

    #[tokio::test]
    async fn ask_completion_is_recognised_once_running_is_false_and_the_entry_is_fresh() {
        let (path, listener) = fake_server().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (mut reader, mut write_half) = {
                let (r, w) = stream.into_split();
                (BufReader::new(r), w)
            };
            write_half
                .write_all(&write_line(&ServiceMsg::Status(Box::default())))
                .await
                .expect("initial status");
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read ask request");
            let request: UiRequest = serde_json::from_str(line.trim()).expect("decode");
            let request_id = request.request_id.expect("has id");
            // Immediate accept.
            write_half
                .write_all(&write_line(&ServiceMsg::CommandResult(CommandResult {
                    request_id,
                    ok: true,
                    message: "queued".to_string(),
                })))
                .await
                .expect("accept result");
            // One in-progress snapshot (running: true) the client must not settle on...
            write_half
                .write_all(&write_line(&ServiceMsg::Status(Box::new(StatusPayload {
                    ask: Some(AskStatus {
                        running: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }))))
                .await
                .expect("in-progress status");
            // ...then the completed answer.
            write_half
                .write_all(&write_line(&ServiceMsg::Status(Box::new(StatusPayload {
                    ask: Some(AskStatus {
                        running: false,
                        entries: vec![AskEntry {
                            question: "what is eating memory".to_string(),
                            answer: "nothing unusual".to_string(),
                            at: i64::MAX, // always "fresh" relative to any sent_at in this test
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                }))))
                .await
                .expect("completed status");
        });

        let _env = set_socket_env(&path).await;
        let args = Args {
            json: false,
            timeout: Some(Duration::from_secs(5)),
            positional: vec!["ask".to_string(), "what is eating memory".to_string()],
        };
        // cmd_ask_or_investigate prints to stdout on success; we only assert it
        // returns promptly rather than hanging on the in-progress snapshot.
        let _ = timeout(Duration::from_secs(5), cmd_ask_or_investigate(&args, false))
            .await
            .expect("must settle once the completed answer arrives, not hang");

        server.await.expect("fake server task");
        let _ = std::fs::remove_file(&path);
    }
}
