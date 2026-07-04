## Projects

Eir — Rust/Tauri v2 Windows desktop agent. Current release is v0.23.1. The workspace has three crates:

- `eir-proto`: shared serde wire contract for the UI/service named pipe.
- `eir-svc`: LocalSystem Windows service that collects signals, calls AI providers, gates actions through policy, executes fixes, runs app updates, and owns the SQLite audit DB.
- `eir-ui`: Tauri tray app using committed static frontend files in `ui/`; no npm/Vite build step.

Canonical build config is `eir-ui/tauri.conf.json`. The stale root `tauri.conf.json` and dead root `build.rs` were removed in v0.23.0 (resolving the long-standing open question).

## Architectural decisions

2026-07-04 | Eir | v0.23.1 second-sweep fixes | A second adversarial sweep confirmed the v0.23.0 registry gate, AI/wire layer, and loop concurrency are sound. Three fixes: LogCleanup now canonicalises its scan root (an 8.3 short-name or junction root could lexically dodge the protected-dir check yet resolve to System32); the first-ever weekly-digest OS notification is no longer suppressed on fresh installs; and the digest prompt no longer frames the all-time learned-fact count as a weekly figure. `usage_log` pruning was deliberately skipped (it feeds the lifetime usage card).

2026-07-03 | Eir | v0.23.0 bug-fix + feature sweep | Fixed the registry allowlist to component-boundary matching (a sibling key sharing a name prefix could bypass it), scoped LogCleanup so it can't recurse into system dirs, fixed a CLI-provider stdin/stdout pipe deadlock, and enabled SQLite WAL. Added: Anthropic prompt caching (static system prompt sent once as a cached block), registry-reset undo (prior value snapshotted → one-click revert), approval + weekly-digest OS notifications, SMART disk-health signal, DISM/SFC repair actions (approval-gated, never whitelisted, own long timeout), a resource-trend signal from the previously-unused system_state_history, and a weekly plain-English health digest. A CI step now fails on version drift across the four manifests.

2026-06-26 | Eir | Keep UI and service as separate processes joined only by newline-delimited JSON over `\\.\pipe\EirSvc` | This keeps LocalSystem repair authority in the service while the medium-integrity tray app remains a thin renderer/command surface.

2026-06-26 | Eir | Generated build artifact is the service binary, staged by `eir-ui/build-svc.ps1` through Tauri `beforeBuildCommand` | `eir-ui/bin/eir-svc.exe` is gitignored but required as a bundle resource, so CI stages it before clippy/tests and the full Tauri build stages it again.

2026-06-26 | Eir | Self-improvement is conservative-only learned facts | Audit-derived learning may skip, deprioritise, suppress noise, or reduce confidence, but it cannot enable actions or raise confidence; this keeps local adaptation from expanding authority.

2026-06-26 | Eir | Advisor mode is bounded escalation, not model-controlled policy | The AI may ask for deeper analysis or trigger low-confidence escalation, but Rust chooses the configured tier and enforces daily spend/attempt caps.

## Cross-project patterns

Maintain `ARCHITECTURE.md` as the deep technical reference and update it with behavior changes. Keep `CONTEXT.md` short: current state, durable decisions, and open questions only.

For this repo, release versions must stay synchronized across `eir-proto/Cargo.toml`, `eir-svc/Cargo.toml`, `eir-ui/Cargo.toml`, `eir-ui/tauri.conf.json`, and `Cargo.lock`.

## Open questions / deferred decisions

Consider moving learning thresholds/windows/half-lives from constants into config once the current detector behavior has more real-world history.

The resource-trend thresholds (audit `summarise_trend`) and disk-health/SMART wording are heuristic — tune against real machine history.

`network_errors` is now collected defensively (falls back to 0 on any query failure) rather than dropped; confirm the CIM class/properties resolve on the target machines or drop the field.

(Resolved in v0.23.0: stale root `tauri.conf.json`/`build.rs` removed; automated version-sync CI check added — `scripts/check-versions.ps1`.)

## Environment constraints

Primary target is Windows with the MSVC Rust toolchain. CI runs on `windows-latest` and pins Rust 1.95.0 to match `rust-toolchain.toml`.

No JavaScript package manager is part of the build; frontend assets are committed static HTML/JS.
