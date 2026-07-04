## Projects

Eir — Rust/Tauri v2 Windows desktop agent. Current release is v0.24.4. The workspace has three crates:

- `eir-proto`: shared serde wire contract for the UI/service named pipe.
- `eir-svc`: LocalSystem Windows service that collects signals, calls AI providers, gates actions through policy, executes fixes, runs app updates, and owns the SQLite audit DB.
- `eir-ui`: Tauri tray app using committed static frontend files in `ui/`; no npm/Vite build step.

Canonical build config is `eir-ui/tauri.conf.json`. The stale root `tauri.conf.json` and dead root `build.rs` were removed in v0.23.0 (resolving the long-standing open question).

## Architectural decisions

2026-07-04 | Eir | v0.24.4 bug sweep | Three parallel review agents (frontend, UI-Rust/wire, service-interaction), each finding verified against source before fixing. Fixed: startup's post-warmup settle unconditionally cleared the "AI provider not configured" error before the first broadcast (with `ai = None` nothing re-set it — a broken provider looked healthy forever; now gated on `ai.is_some()`); the Settings dirty flag was view-global so saving one card discarded unsaved edits in another on re-entry (now per-card `dirtyCards`); tray repaint failures were swallowed with the change-guard already advanced, freezing the tray on stale state (guard now advances only on success); the hero "Open Settings" regex matched bare "model"/"key" in transient AI errors (tightened to the service's enumerated config-shaped strings + auth failures). Confirmed safe in the same sweep: `ApprovalInfo.reversible` is exhaustively type-derived (never AI-supplied), `open_url`'s single-quoted PowerShell doesn't expand `$()`. Accepted, not fixed: the `ensure_connected`→`try_send` TOCTOU (inherent to fire-and-forget, documented) and a `log_decision`-failure delaying a `last_analysis_at` broadcast by one cycle. Compile-verified (fmt + clippy --all-targets + 191 tests + node --check).

2026-07-04 | Eir | v0.24.3 UI/UX pass | A focused tray-app UX sweep, UI-only except one skew-safe wire field. Safety: approving an irreversible action now takes two clicks (armed orange button, 6 s window; survives the 2 s poll via the existing approvals signature guard). Data-loss guard: Settings tracks a `settingsDirty` flag so re-entering the view no longer refills over unsaved edits; numeric inputs clamp to their declared min/max on save so the sent value matches what the service honours (a mid-range typo no longer silently coerces to the default). Trust signals: `StatusPayload.last_analysis_at` (`#[serde(default)]`) renders "analysed Xm ago" on the dashboard; every relative age gets an absolute-time tooltip. Convenience: Enter sends in Ask Eir (Shift+Enter newline, IME-guarded), Escape hides the window (blurs fields first), config-shaped errors get an "Open Settings" quick link, Disk/Startup headers summarise scans, paused state highlighted in the sidebar, `:focus-visible` rings for keyboard use. Adversarial diff review: one confirmed finding (missing `isComposing` guard) fixed; wire skew, Infinity-serialization, TDZ, and poll-rebuild concerns each refuted. Compile-verified (fmt + clippy --all-targets + `cargo test --workspace` + `node --check`); not live-exercised.

2026-07-04 | Eir | v0.24.2 correctness & hardening sweep (C1–C21) | A fresh multi-agent adversarial sweep (six subsystems, several double-covered) found and fixed 21 items across service loop, AI layer, updater, executor, persistence, and frontend. Load-bearing ones: (C1) approved fixes were abandoned on service stop/restart because the off-loop executor worker's handle was discarded and the Tokio runtime dropped mid-job — now the worker is drained (drop the sole `ExecJob` sender + a 30 s bounded join) before `eir_main` returns, and the SCM handler reports `StopPending`+`wait_hint` so SCM waits (long SFC/DISM remain best-effort). (C2) an AI-supplied `verify_exe` UNC path could force LocalSystem SMB auth — now drive-letter only. (C4) the updater trusted any `github.com` repo for any app — now the owner/repo must correlate with the app name (narrows, not eliminates; Authenticode/SHA is the backstop). (C3) `strip_fences` dropped valid AI responses containing backticks — fixed + raw-JSON fallback. (C7) three audit tables grew unbounded — now pruned to 90 days. Plus C5/C6/C8/C9/C12/C13/C14/C15/C16/C17/C19/C20 and UI C10/C11/C18/C21. Each fix was adversarially re-reviewed against the diff (three review agents) — the C4 repo-name-contains-app-token residual and the shutdown-drain guarantee were documented honestly rather than over-claimed. Gate green: fmt + clippy --all-targets + `cargo test --workspace` (191 tests, incl. 5 new). Shutdown-drain, live SMB, and SCM paths are reasoning-verified, not live-exercised.

2026-07-04 | Eir | v0.24.1 UI/user-facing bug-fix sweep (B1–B10) | Ten UI bugs from `PLAN.md`. Load-bearing one: a click made while the service was down/restarting was accepted by the Tauri command (mpsc `try_send` succeeds) then silently dropped by the pipe client's stale-command drain — and an Approve so lost left its buttons permanently disabled. Fixed with a shared `Arc<AtomicBool>` connected flag set by `pipe_client` and checked by `ensure_connected` in every pipe-facing handler, plus a frontend `svc-down` class that greys/blocks the same buttons. Also: `renderActivity` gained the signature guard the other list renderers already had (2s repaint was wiping text selection and the Undo double-click guard), a `setText` helper stops the dashboard text nodes churning, and relative ages now render as `data-ts` spans re-ticked by one sweep so guarded lists don't freeze at "just now". Tray: `unminimize()` on open/click restores a minimized window; the pause menu entry relabels Resume↔Pause; the tooltip humanises CamelCase via `friendly_status()`. Service: an ignore-toggle's empty note now means "unchanged" (was wiping config-set notes). Min window size 720×480. New unit tests: `friendly_status`, `ensure_connected`, and the connected-flag flip. Compile-verified (fmt + clippy --all-targets + `cargo test --workspace` green); tray menu mutation and window restore are compile-only, not live-exercised.

2026-07-04 | Eir | v0.24.0 user-facing tools (F10–F13) | Added four on-demand features: a dashboard health timeline (reads the previously UI-invisible `system_state_history`), "Ask Eir" free-text Q&A (bounded `complete_text`, diagnostic-only — nothing parsed/executed from the answer), disk-space insights ("what's eating my SSD", cleanup mapped only to existing safe actions), and a startup advisor (Run keys + Startup folders, StartupApproved toggle via the new reversible `FixAction::StartupSet`, AI classify is advisory-only). Trust boundary held: UI sends only opaque ids, the service reconstructs the action from its own last-scan state and routes through the same policy gate (`route_user_action`). An adversarial multi-lens sweep found no security/concurrency defects; three UX-feedback papercuts (rate-limited/stale-id/paused clicks) were fixed. Self-review also caught that `executor::startup::approved_key` must `Registry::`-qualify keys (`HKEY_USERS` has no PSDrive).

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
