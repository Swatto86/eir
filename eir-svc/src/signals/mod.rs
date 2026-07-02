pub mod event_log;
pub mod file_watch;
pub mod log_parser;
pub mod wmi;

/// Reactive-guardian trigger: collectors ping this (capacity-1, `try_send`, so a
/// burst coalesces and a send never blocks) when they capture something
/// actionable — an Error/Warning event, an error-bearing log write, a failed
/// service or security fault. The decision loop reacts within seconds instead
/// of waiting for the next scheduled tick.
pub type TriggerTx = tokio::sync::mpsc::Sender<()>;
