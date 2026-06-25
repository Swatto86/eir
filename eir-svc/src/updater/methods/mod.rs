//! Per-method update adapters. Each module knows how to attempt an update for one
//! backend and report a structured [`super::domain::AttemptOutcome`]. The
//! orchestrator dispatches to them by [`super::domain::Method`] (exhaustive match).

pub mod native;
