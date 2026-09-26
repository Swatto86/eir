pub mod event_log;
pub mod feed;
pub mod file_watch;
// Only file_watch.rs's Windows log-tail parser (`try_parse_log`) uses this — Linux
// file watching is journald-first for v1 (see `event_log`), so it has no caller there.
#[cfg(windows)]
pub mod log_memory;
#[cfg(windows)]
pub mod log_parser;
pub mod profile;
pub mod screen;
pub mod wmi;

/// Reactive-guardian trigger: collectors ping this (capacity-1, `try_send`, so a
/// burst coalesces and a send never blocks) when they capture something
/// actionable — an Error/Warning event, an error-bearing log write, a failed
/// service or security fault. The decision loop reacts within seconds instead
/// of waiting for the next scheduled tick.
pub type TriggerTx = tokio::sync::mpsc::Sender<()>;
