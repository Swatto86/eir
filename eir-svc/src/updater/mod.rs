//! App-update subsystem: AI-driven, self-healing, unattended updates across
//! winget, Chocolatey, Scoop, the Microsoft Store, and AI-found native installers.
//!
//! Layered internally, dependencies pointing inward:
//!   - a pure domain/validation core (no I/O) that every AI proposal must pass
//!     before anything runs — "AI proposes, Rust disposes";
//!   - an application orchestrator (the check -> attempt -> diagnose -> retry loop);
//!   - per-method infrastructure adapters (winget/choco/scoop/msstore/native).
//!
//! It runs inside the LocalSystem service, so package managers and installers run
//! with no UAC prompt.

pub mod check;
pub mod config;
pub mod diagnose;
pub mod domain;
pub mod download;
pub mod history;
pub mod methods;
pub mod names;
pub mod orchestrator;
pub mod plan;
pub mod verify;
pub mod version;
pub mod winget_parse;
