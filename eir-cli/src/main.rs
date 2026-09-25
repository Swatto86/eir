//! `eirctl` — the command-line control surface for the headless Linux Eir guardian.
//!
//! Talks the exact same `UiRequest` / `ServiceMsg` / `CommandResult` / `StatusPayload`
//! JSON-line wire protocol that eir-svc's Windows tray pipe uses (see `eir-proto`), but
//! over a Unix domain socket (`service.socket_path`, default `/run/eir/eir.sock`) instead
//! of a named pipe — no wire-format change at all.
//!
//! eir-svc never runs on Windows in a form `eirctl` could control (there is no headless
//! Linux guardian there), so the Windows build of this crate is a two-line stub that
//! exists only so `cargo clippy`/`cargo test --workspace` stay green on the Windows CI
//! job — matching every other workspace member.

#[cfg(unix)]
mod unix;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    unix::run()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "eirctl controls the headless Linux build of eir-svc over a Unix domain socket \
         and has nothing to connect to on this platform."
    );
    std::process::ExitCode::from(1)
}
