## Projects

Eir — Rust/Tauri v2 Windows desktop agent. Current release is v0.16.0. The workspace has three crates:

- `eir-proto`: shared serde wire contract for the UI/service named pipe.
- `eir-svc`: LocalSystem Windows service that collects signals, calls AI providers, gates actions through policy, executes fixes, runs app updates, and owns the SQLite audit DB.
- `eir-ui`: Tauri tray app using committed static frontend files in `ui/`; no npm/Vite build step.

Canonical build config is `eir-ui/tauri.conf.json`. The root `tauri.conf.json` still exists but is not the documented build path.

## Architectural decisions

2026-06-26 | Eir | Keep UI and service as separate processes joined only by newline-delimited JSON over `\\.\pipe\EirSvc` | This keeps LocalSystem repair authority in the service while the medium-integrity tray app remains a thin renderer/command surface.

2026-06-26 | Eir | Generated build artifact is the service binary, staged by `eir-ui/build-svc.ps1` through Tauri `beforeBuildCommand` | `eir-ui/bin/eir-svc.exe` is gitignored but required as a bundle resource, so CI stages it before clippy/tests and the full Tauri build stages it again.

2026-06-26 | Eir | Self-improvement is conservative-only learned facts | Audit-derived learning may skip, deprioritise, suppress noise, or reduce confidence, but it cannot enable actions or raise confidence; this keeps local adaptation from expanding authority.

2026-06-26 | Eir | Advisor mode is bounded escalation, not model-controlled policy | The AI may ask for deeper analysis or trigger low-confidence escalation, but Rust chooses the configured tier and enforces daily spend/attempt caps.

## Cross-project patterns

Maintain `ARCHITECTURE.md` as the deep technical reference and update it with behavior changes. Keep `CONTEXT.md` short: current state, durable decisions, and open questions only.

For this repo, release versions must stay synchronized across `eir-proto/Cargo.toml`, `eir-svc/Cargo.toml`, `eir-ui/Cargo.toml`, `eir-ui/tauri.conf.json`, and `Cargo.lock`.

## Open questions / deferred decisions

Decide whether to remove or convert the stale root `tauri.conf.json` into an explicit shim so contributors do not build with the wrong config.

Add an automated version-sync check for the three crate manifests and `eir-ui/tauri.conf.json`.

Consider moving learning thresholds/windows/half-lives from constants into config once the current detector behavior has more real-world history.

## Environment constraints

Primary target is Windows with the MSVC Rust toolchain. CI runs on `windows-latest` and pins Rust 1.95.0 to match `rust-toolchain.toml`.

No JavaScript package manager is part of the build; frontend assets are committed static HTML/JS.
