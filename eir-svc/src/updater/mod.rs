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
//! with no UAC prompt. Building out across phases.
//
// The subsystem is assembled across phases: the pure core and adapters land before
// the orchestrator and loop that consume them, so parts are intentionally unused
// mid-build. This blanket allow (which propagates to child modules) keeps each
// phase green under `clippy -D warnings`; it is removed once the engine is wired
// into the service loop (Phase 8), at which point any genuinely dead code surfaces.
#![allow(dead_code)]

pub mod config;
pub mod domain;
pub mod download;
pub mod methods;
pub mod names;
pub mod plan;
pub mod verify;
pub mod version;
pub mod winget_parse;
