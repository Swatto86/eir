<!--
  LIVING DOCUMENT — keep this current with every architectural change.
  Update the relevant section (and the "Last updated" line) in the same commit that
  changes behaviour. Sections below were machine-mapped from the source and then
  curated; treat the code as ground truth and correct this doc when they diverge.
-->

# Eir — Architecture & Design

**Last updated:** 2026-07-27 · **Release:** v0.33.0

Eir is an autonomous Windows system guardian: it watches a machine's health,
uses an AI model to diagnose problems **as they happen** (event-driven, not just
polled), and applies least-destructive fixes — auto-running reversible
whitelisted repairs and queuing anything disruptive for approval. It also keeps
installed apps up to date unattended.

## Overview

Eir runs as **two cooperating processes**:

- **`EirSvc`** — a Windows service running as **LocalSystem** (`eir-svc`). It collects
  signals, calls the AI to diagnose, gates findings through policy, executes approved
  fixes, runs the autonomous updater, and owns the SQLite audit DB. Running as
  LocalSystem lets it read protected logs and apply fixes with no UAC prompt.
- **Eir tray app** — a lightweight Tauri v2 desktop UI (`eir-ui`) that shows status,
  approvals, AI usage, and app updates, and is where every setting is changed.

They never link against each other; they communicate only through the `eir-proto`
wire contract (newline-delimited JSON) over the secured local named pipe
`\\.\pipe\EirSvc`. Layering points inward: `eir-proto` (pure contract types) ←
`eir-svc` / `eir-ui` (each depends only on `eir-proto`).

**The decision cycle** runs on two triggers: a scheduled sweep (default every
10 min, `decision_interval_secs`) and a **reactive path** — each signal collector
pings a capacity-1 trigger channel the moment it captures something actionable
(an Error event-log entry, an error-bearing log write, a new failed service or
security fault), and the loop *schedules* a reaction ~10 s later (debounce, so a
burst coalesces into one analysis) with a 60 s minimum gap between reactions.
The cycle itself: collect signals → compute an *actionable fingerprint* and only
call the AI when something actionable changed (plus a periodic heartbeat) → AI
returns structured problems each with a confidence and a proposed `FixAction` →
policy gates each (auto-execute / require-approval / block) → reversible
whitelisted fixes at/above the confidence threshold run on an **off-loop
executor worker**; disruptive/irreversible ones queue for approval. Every
decision, execution, approval, and update attempt is persisted to the audit DB —
which is the substrate the self-improvement layer learns from (see
[Self-improvement](#self-improvement-machine-pattern-learning)).

## Table of contents

- [Workspace, build & delivery pipeline](#workspace-build--delivery-pipeline)
- [Pipe protocol & tray UI](#pipe-protocol--tray-ui)
- [Service decision loop, state & off-loop executor](#service-decision-loop-state--off-loop-executor)
- [Signal sources](#signal-sources)
- [AI layer & prompts](#ai-layer--prompts)
- [Executor, policy, safety & explanations](#executor-policy-safety--explanations)
- [Autonomous app updater](#autonomous-app-updater)
- [User-facing on-demand tools (Ask / Timeline / Disk / Startup)](#user-facing-on-demand-tools-ask--timeline--disk--startup)
- [Persistence, audit DB & the existing feedback loop](#persistence-audit-db--the-existing-feedback-loop)
- [Self-improvement: machine-pattern learning](#self-improvement-machine-pattern-learning)
- [Current limitations and roadmap](#current-limitations-and-roadmap)

---


## Workspace, build & delivery pipeline

Eir is a single Cargo workspace (`resolver = "2"`) with three crates, plus a static, hand-written frontend and a Tauri-driven NSIS delivery pipeline. There is **no JavaScript toolchain** — no `package.json`, no bundler, no `npm` step anywhere — which shapes the entire build.

### Crate layout & layering

`Cargo.toml` (repo root) declares `members = ["eir-proto", "eir-svc", "eir-ui"]`. Mapping to the layering model (per `README.md` "Project layout", confirmed in each `Cargo.toml`):

| Crate | Layer | Binary | Responsibility |
|-------|-------|--------|----------------|
| `eir-proto` | shared/contract | (lib) | Wire types for the UI↔service named-pipe protocol (serde, snake_case). Pure types, no I/O. Depended on by both other crates (`eir-proto = { path = "../eir-proto" }`). |
| `eir-svc` | infrastructure/service | `eir-svc` (`src/main.rs`) | LocalSystem Windows service: signal collection, AI client, policy, execution, autonomous updater, SQLite audit DB. Heavy `windows` 0.58 feature set. |
| `eir-ui` | presentation/composition root | `eir` (`src/main.rs`) | Tauri v2 tray app. Wires the system together and renders status/approvals/updates. Deps: `tauri` 2 (`tray-icon`), `tauri-plugin-autostart` 2, `tauri-plugin-updater` 2, `tokio` (full), `image` (png), `windows-service` 0.7 (SCM queries + install from About), tracing. `build-dependencies`: `tauri-build` 2. |

All three crates are versioned in lockstep — currently `0.33.0` in every `[package] version` (`eir-proto/Cargo.toml:3`, `eir-svc/Cargo.toml:3`, `eir-ui/Cargo.toml:3`), matching `eir-ui/tauri.conf.json`. `scripts/check-versions.ps1` gates CI on all four agreeing.

The dependency graph is acyclic and points inward: `eir-proto` depends on nothing internal; `eir-svc` and `eir-ui` each depend only on `eir-proto`. The UI and service never link against each other — they are separate processes coupled solely through the `eir-proto` wire contract over `\\.\pipe\EirSvc`.

`build.rs` (repo root, `eir-ui`'s — `eir-ui/build.rs` is 42 bytes, the root `build.rs` shown is `tauri_build::build()`) is the standard Tauri build hook that runs `tauri_build::build()` to validate the bundle config and embed the frontend at compile time.

### Frontend: static, committed, no build step

`frontendDist` is `"../ui"` (relative to `eir-ui/`), which resolves to the **repo-root `ui/` directory**, not `eir-ui/ui/`. That directory contains exactly two committed, hand-written files: `ui/index.html` and `ui/main.js`. `tauri.conf.json` sets `withGlobalTauri: true`, so `main.js` calls the Tauri API off the global object rather than importing an npm module — no bundler, no transpile, no generated CSS. This is the key reason `beforeBuildCommand` compiles only the service (there are no frontend assets to generate). It does diverge from the user's WattMail blueprint preference for "vanilla TS + Vite"; here it's plain JS with no Vite.

### The beforeBuildCommand chain (service staging)

`tauri.conf.json` build block:
- `beforeBuildCommand`: `powershell -NoProfile -ExecutionPolicy Bypass -File eir-ui\build-svc.ps1`
- `beforeDevCommand`: `""` (empty)

`eir-ui/build-svc.ps1` is the single generated-artifact step. It:
1. `cargo build -p eir-svc --release` (exits 1 on failure).
2. Resolves the workspace target dir via `cargo metadata --no-deps` → `.target_directory`.
3. Copies `<target>/release/eir-svc.exe` → `eir-ui/bin/eir-svc.exe`, creating `bin/` if absent.

`eir-ui/bin/` is **gitignored** (`.gitignore:4` `eir-ui/bin/`; `git check-ignore` confirms `eir-ui/bin/eir-svc.exe` is ignored). So the staged service binary is a build-time artifact, never committed. `tauri.conf.json` `bundle.resources` then pulls it into the installer:
```
"bin/eir-svc.exe": "eir-svc.exe",
"../config.toml.example": "config.toml.example",
"../policy.toml": "policy.toml"
```
This means `eir-svc.exe` ends up at the install root (renamed from `bin/`), alongside the config template and policy file. This is the project-specific application of the user's Tauri rule "wire `beforeBuildCommand` to every generated-frontend-asset step" — here the only generated artifact is the service binary, so that's what the hook builds.

**Build data flow:** `cargo tauri build` (in `eir-ui`) → runs `build-svc.ps1` (compiles + stages `eir-svc.exe` into `eir-ui/bin/`) → `tauri_build`/`tauri-codegen` embeds `ui/` HTML+JS into the `eir` binary → NSIS bundler packages `eir.exe` + `eir-svc.exe` + `config.toml.example` + `policy.toml` + installer hooks into `Eir_<version>_x64-setup.exe`, plus signed updater artifacts (`createUpdaterArtifacts: true`).

### Toolchain pinning

`rust-toolchain.toml` at **repo root** pins `channel = "1.95.0"` — correctly at root (not under a crate) so rustup resolves it from any cwd. Both CI workflows pin `dtolnay/rust-toolchain@1.95.0` to match exactly (with an explanatory comment in `ci.yml:18-19`), satisfying the user's "pin Rust toolchain + match CI to local" rule.

### CI gate (`.github/workflows/ci.yml`)

Triggers: `push` to `master`, and all `pull_request`. `permissions: contents: read`. Single job `verify` on `windows-latest`:
1. `actions/checkout@v6`, then `scripts/check-versions.ps1`.
2. `dtolnay/rust-toolchain@1.95.0` with `rustfmt, clippy`, then `swatinem/rust-cache@v2`.
3. **Format**: `cargo fmt --all --check`.
4. **Stage service binary**: runs `build-svc.ps1` — required because `eir-ui`'s `tauri_build` validates bundle resources during clippy/tests.
5. **Clippy/test**: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace --all-targets`.
6. **Full Tauri build**: `tauri-apps/tauri-action@v0` builds and signs the real bundle without publishing.
7. **Installed-service smoke**: installs the staged binary as `EirSvc`, verifies it is running as LocalSystem, negotiates protocol capabilities, and requires a successful correlated pause command over the real pipe.
8. **Standalone smoke**: launches `target/release/eir.exe` and requires it to remain running.

The early staging and the bundle build's `beforeBuildCommand` both build the service; this is intentional because clippy/tests need the resource before the bundle step.

The green CI gate is the only pre-release bar (matches the user's "no manual/live-test gate" policy).

### Tag-driven signed release (`.github/workflows/release.yml`)

Triggers: `push` of tags matching `v*`. `permissions: contents: write`. Single job `release` on `windows-latest`:
1. `actions/checkout@v6`
2. `dtolnay/rust-toolchain@1.95.0` (no components — release doesn't lint)
3. `swatinem/rust-cache@v2`
4. `tauri-apps/tauri-action@v0` with `projectPath: eir-ui`, `tagName: ${{ github.ref_name }}`, `releaseName: Eir ${{ github.ref_name }}`, a templated `releaseBody`, `releaseDraft: true`, `prerelease: false`. Env: `GITHUB_TOKEN` + the two `TAURI_SIGNING_*` secrets.
5. A PowerShell step uploads the portable executable and checksums, verifies the installer, updater metadata/signature, portable binary, and checksum assets, then publishes the draft.

Because `tagName` is set, `tauri-action` builds, signs, and creates a **draft** GitHub release with the NSIS installer (`Eir_<version>_x64-setup.exe`), `latest.json`, and the `.sig`. The release is not public until every required asset has been verified. The signing keypair is minisign; the public key is embedded in `tauri.conf.json` `plugins.updater.pubkey` (base64 minisign public key).

### Self-update wiring (single rolling release)

`tauri.conf.json` `plugins.updater.endpoints` is a single URL:
`https://github.com/Swatto86/eir/releases/latest/download/latest.json`. It points at the **`/latest/`** redirect, so the installed app always fetches whichever release is newest — the single-rolling-release model. `createUpdaterArtifacts: true` ensures `latest.json` + `.sig` ship beside the installer. The unauthenticated `/latest/download/` fetch requires the release repo to be public.

The NSIS install hooks (`eir-ui/installer-hooks.nsh`, wired via `bundle.windows.nsis.installerHooks`, `installMode: perMachine`) make self-update actually work over a running service:
- **PREINSTALL**: `sc stop EirSvc`, poll for STOPPED for up to ~40 s, then `sc delete EirSvc` before files are replaced.
- **POSTINSTALL**: seed `config.toml` from `config.toml.example` on first install, delete the template, then run `eir-svc.exe install`; the install verb registers and starts the service.
- **PREUNINSTALL**: stop/unregister the service, then remove the current user's Eir autostart values and Tauri config directory.
- **POSTUNINSTALL**: empty.

`bundle.windows.allowDowngrades: false`; targets `["nsis"]` only; main window `visible: false`. Manual launches show/focus it during Tauri setup; Windows-login autostart passes `--hidden` so it stays in the tray.

### Version-bump locations

A release version lives in **four** places that must move together:
- `eir-ui/tauri.conf.json` → `"version"` (drives installer filename, About, updater compare).
- `eir-proto/Cargo.toml`, `eir-svc/Cargo.toml`, `eir-ui/Cargo.toml` → `[package] version`.
- `Cargo.lock` must be re-synced (it is present and tracked, 152 KB).

There is **no `package.json`**, so the WattMail blueprint's "bump `package.json`" step does not apply here. The release-commit `[release]` marker convention is visible in git history (e.g. `0752c08 ... (v0.10.2) [release]`).

### Build/release control flow (summary)

Developer bumps the 3 `Cargo.toml` versions + `tauri.conf.json` + `Cargo.lock`, commits with `[release]`, pushes to `master`, waits for CI on that exact SHA, then pushes tag `vX.Y.Z` → `release.yml` builds the signed bundle as a draft, verifies every asset, and publishes the rolling GitHub release → installed clients poll `releases/latest/download/latest.json`, verify the minisign `.sig` against the embedded pubkey, and self-update (NSIS hooks stop/replace/restart `EirSvc`).

## Pipe protocol & tray UI

The UI subsystem is a thin Tauri tray app (`eir-ui`) that talks to the LocalSystem service (`eir-svc`) over a single Windows named pipe, `\\.\pipe\EirSvc`. All wire types live in the shared `eir-proto` crate so both ends serialize/deserialize the same shapes. The service owns all state; the UI renders a locally-cached snapshot and correlates mutating commands with service outcomes when protocol v2 is available.

### Transport & framing

- **Pipe name**: `eir_proto::PIPE_NAME = r"\\.\pipe\EirSvc"` (`eir-proto/src/lib.rs:3`), used by both server (`pipe_server.rs:55`) and client (`pipe_client.rs:16`).
- **Framing**: newline-delimited JSON ("JSON lines"). Each direction writes one `serde_json` object per line terminated with `\n`; readers use `BufReader::read_line` and `serde_json::from_str` on the trimmed line. Pipe mode is **byte stream** (`PipeMode::Byte`, `pipe_server.rs:87`), not message mode — framing is purely the newline.
- **Bidirectional, split**: on both ends the connected pipe is `tokio::io::split` into an independent reader and writer. The service runs the writer as a spawned task and the reader inline (`pipe_server.rs:126-182`); the client runs both as `async` blocks joined by `tokio::select!` (`pipe_client.rs:63-113`).

### Wire types (`eir-proto/src/lib.rs`)

Two tagged enums carry service output, while a flattened request wrapper preserves the original command shape:

- **`ServiceMsg`** (service → UI) carries either `Status(StatusPayload)` or a correlated `CommandResult { request_id, ok, message }`.
- **`UiRequest`** flattens an optional `request_id` beside the existing tagged **`UiMsg`**, so old services can still deserialize new commands and new services can acknowledge whether a command was actually applied. The tray falls back to neutral queued feedback when connected to protocol v1.
- **`UiMsg`** includes approval, pause, settings, refresh, updater, learned-fact, Ask, disk, startup, Game Mode, and `TestProvider`. `TestProvider` exercises the saved provider/model from the LocalSystem service context without exposing credentials. `RefreshStatus` forces an immediate services-only rescan and status re-settle.

**`StatusPayload`** is the single snapshot the UI renders. In addition to health, activity, settings, updater/advisor, on-demand-tool, and learned-fact state, protocol v2 adds `protocol_version`, `capabilities`, `svc_version`, `signals_at`, and `signal_errors`. All new fields default so an old peer remains decodable; unavailable metrics render as unknown rather than healthy zeroes.

Supporting types:
- **`ApprovalInfo`** (`lib.rs:226-259`): `id: u64`, `diagnosis`, `root_cause`, `confidence: f32`, `action` (debug render of the fix), `reason` (policy verdict), `side_effects`, `undo_instructions`, plus the trust-critical deterministic fields `action_summary`, `target`, `target_details`, `reversible: bool`, `created_at: i64`. The doc comment notes `action_summary` is "derived from the action type, not the AI, so it can be trusted."
- **`ProblemSummary`** (`lib.rs:261-275`): `diagnosis`, `confidence`, `action`, `blocked`, `auto_executed`, `reason: Option<String>`, `at: i64`.
- **`ExecutionSummary`** (`lib.rs:277-285`): `action`, `success`, `preview`, `at: i64`.
- **`UpdaterStatus`**: `enabled`, `running`, `phase`, durable `last_run`/`last_clean_run`, `next_run`, cost/notes, saved app guidance, per-app results, rich recent attempts, and settings. A current app is distinct from an installed/verified update, and incomplete source coverage prevents a clean-cycle claim.
- **`LearnedFactView`** (`lib.rs:42-60`): `id`, `summary`, `detail`, `status`, `source` for the UI's "What Eir has learned" card.
- **`AdvisorStatus`** (`lib.rs:64-77`): `enabled`, `escalated`, `escalation_model`, `reason`, `spent_today_usd`, `settings: AdvisorSettingsView`.
- **`UiSettings`** / **`UsageSummary`** plus the `*Update` mirrors (`SettingsUpdate`, `UpdaterSettingsUpdate`, `AdvisorSettingsUpdate`) that flow back as `UiMsg` payloads. The provider settings carry `openrouter`/`anthropic` key flags and secrets, plus `kilo_cli_user_profile`/`kilo_cli_path` hint fields; the removed OpenAI-compatible provider's `base_url`/`api_key_set` remain on `UiSettings` as always-empty **deprecated wire fields** so a not-yet-updated v0.16 tray app (which requires them) can still decode the payload during an update's skew window. The API-key-based `kilocode` gateway provider (and its `kilocode_api_key`/`kilocode_key_set` wire fields) was removed in v0.19 in favour of the subscription-based `kilo_cli` path — an old config's `provider = "kilocode"` now aliases to `kilo_cli` on load rather than failing to parse.

**Backward-compat invariant**: additive status fields use `#[serde(default)]`, and `UiRequest.request_id` is optional + flattened. This keeps the original top-level command JSON valid across the installer’s UI/service skew window; capabilities tell the tray when it may rely on command results or provider testing.

**Secret-handling invariant**: `UiSettings` never carries secret values, only booleans (`openrouter_key_set`, `anthropic_key_set`) so the UI shows "configured" without exposing keys. OpenRouter and native Anthropic take pasted API keys; `claude_cli`, `codex_cli`, and `kilo_cli` borrow a locally logged-in subscription session instead. Inbound `SettingsUpdate` uses `Option<String>` for secrets where `None` = "unchanged" and a non-empty value replaces the stored secret; the JS sends `null` to preserve.

### Named-pipe security / ACL model (`pipe_server.rs:16-48`)

A pipe created by a LocalSystem service defaults to granting only SYSTEM + Administrators, so a non-elevated, medium-integrity UI would get "Access is denied." `build_pipe_security_descriptor()` builds an explicit descriptor from the SDDL string:

```
D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)S:(ML;;NW;;;ME)
```

- **DACL**: SYSTEM (`SY`) and Administrators (`BA`) get full control; Interactive Users (`IU`) get read/write. Every accepted connection is also resolved to a client process/session and rejected unless it belongs to the current active interactive session; the session is rechecked during reads and writes, so a user switch invalidates the old client.
- **SACL mandatory label**: `S:(ML;;NW;;;ME)` sets a **Medium** integrity label with the no-write-up (`NW`) policy. Without this the pipe inherits the creator's System integrity and Windows' no-write-up rule would silently let the medium-integrity UI *read* but block its *writes* (Approve/Reject/Pause). Labelling the pipe Medium permits the UI's writes while still blocking Low-integrity (sandboxed) processes.

Implementation details: the descriptor is **intentionally leaked** and returned as a `usize` (not a raw pointer) so it can cross the listener's `.await` points (a raw pointer is not `Send`); `pipe_server.rs:32,47`. The `SECURITY_ATTRIBUTES` is constructed inside a block that ends before the first `.await`, so the non-`Send` pointer is never held across an await (`pipe_server.rs:83-104`), and the pipe is created via `create_with_security_attributes_raw`. If descriptor construction fails the server falls back to a default-ACL pipe and warns (`pipe_server.rs:77-79, 102`).

### Service side: listener & broadcast (`pipe_server.rs`)

- `spawn()` creates a `watch::channel<StatusPayload>` (status fan-out) and an `mpsc::channel<UiMsg>` (size 8, command intake), returns a `PipeServer { status_tx }` plus the `UiMsg` receiver, and spawns `listener_task` (`pipe_server.rs:54-69`).
- `PipeServer::broadcast_status()` just does `status_tx.send()` — the rest of the service pushes a fresh snapshot here whenever state changes (`pipe_server.rs:187-191`).
- **Listener loop**: connects one client, enforces the active-session identity, then splits it. The writer sends a protocol-v2 starting snapshot immediately and broadcasts status/command results; the bounded reader rejects oversized lines, deserializes `UiRequest`, and forwards commands plus correlation ids to the decision loop. Bad messages are logged and skipped.
- **Single-consumer design**: the listener handles one connection at a time; on disconnect (`read_line` returns `Ok(0)` EOF) it aborts the writer task and loops back to accept the next client (`pipe_server.rs:182-184`). There is no concurrent multi-client support.
- **Critical invariant**: Approve/Reject are resolved only by the decision loop against the persistent queue. Claiming is atomic in SQLite; approved-but-not-completed rows reload after a service restart, while rejected rows cannot resurrect.

### UI side: pipe client (`eir-ui/src/pipe_client.rs`)

- `run(status: SharedStatus, cmd_rx, connected: Arc<AtomicBool>)` loops forever, reconnecting on disconnect with a 5s backoff. `SharedStatus = Arc<Mutex<StatusPayload>>` is the locally-cached snapshot the Tauri command reads.
- **Connected flag (v0.24.1)**: `connect_and_run` stores `true` into the shared `connected` flag the moment the pipe opens and `run_on` stores `false` the moment the connection ends. Command handlers gate on it via `ensure_connected` (below), so a click made while the service is down or mid-restart fails loudly instead of being queued and then silently dropped by the stale-command drain. The flag flip is covered by a `pipe_client` unit test.
- On disconnect it overwrites the cached status to `"ServiceDisconnected"` with an actionable error string telling the user to install/start the service. A clean EOF (settings-save restart) instead shows `"Connecting"`; both are among the frontend's `svc-down` states.
- `connect_and_run` opens the client with retry handling: `ERROR_FILE_NOT_FOUND` (os err 2) → bubble up (pipe not created yet, triggers reconnect); `ERROR_PIPE_BUSY` (231) → 50ms retry; other errors → propagate (`pipe_client.rs:45-59`).
- **Read and write are separate loops joined by `select!`** (`pipe_client.rs:71-112`). The read loop deserializes `ServiceMsg::Status` and replaces `*status.lock()` wholesale. The write loop drains `cmd_rx` and writes each command as a JSON line + flush. A documented design note explains *why* they're separate: a previous single-`select!` design cancelled the in-flight `read_line` on every command send, and `read_line` is not cancellation-safe, corrupting the status stream and starving writes (`pipe_client.rs:66-70`).

### Tauri command surface (`eir-ui/src/main.rs`)

Managed state: `SharedStatus` (the cached payload), `UiCmdTx(mpsc::Sender<UiMsg>)`, and `ConnState(Arc<AtomicBool>)` (the pipe-connected flag, v0.24.1). The `main()` wiring creates the channel (size 16) and the flag, seeds the cache to `"Connecting"`, spawns `pipe_client::run` with a clone of the flag, and registers the command handlers. Each pipe-facing handler calls `ensure_connected(&conn.0)?` (returns `Err("Eir service is not connected")` when the flag is false) before `try_send`; UI-local commands (`get_status`, autostart, `util::*`, version/update-check) are ungated.

Commands (`main.rs:28-112`, plus `util.rs`):
- `get_status` — **synchronous**, returns a clone of the cached `StatusPayload`. This is the UI's only read path; it never hits the pipe directly.
- `decide_approval { id, approved }` → `UiMsg::Approve`.
- `set_learned_fact { id, op }` → `UiMsg::SetLearnedFact` (`op` is `pin`, `disable`, or `forget`).
- `toggle_pause` → `UiMsg::TogglePause`.
- `clear_problems` / `clear_executions` → `ClearProblems` / `ClearExecutions`.
- `refresh_status` → `RefreshStatus` (Dashboard "Refresh" in the Failed Services card; forces a services rescan so a recovered service clears immediately).
- `update_settings(SettingsUpdate)` → `UpdateSettings`.
- `run_updates_now` → `RunUpdatesNow`.
- `clear_update_history` → `ClearUpdateHistory`.
- `set_updater_settings(UpdaterSettingsUpdate)` → `UpdateUpdaterSettings`.
- `set_app_ignore { id, ignore, note }` → `SetAppIgnore`; `set_app_note { id, note }` → `SetAppNote`.
- `set_advisor_settings(AdvisorSettingsUpdate)` → `SetAdvisorSettings`.
- `get_autostart_enabled` / `set_autostart_enabled { enabled }` — UI-local commands backed by `tauri-plugin-autostart`; they never cross the service pipe.
- `ask_eir { question }`, `clear_ask`, `scan_disk`, `clean_disk_entry { id }`, `scan_startup`, `set_startup_entry { id, enable }` (v0.24.0 / v0.29.0) — the on-demand-tools commands, each sent as a correlated `UiMsg` (`AskEir`/`ClearAsk`/`ScanDisk`/`CleanDiskEntry`/`ScanStartup`/`SetStartupEntry`). The clean/toggle commands carry only an opaque id; the service reconstructs the action from its own last-scan state.
- `get_app_version` (About view; reads the package version from the build), `get_service_state`/`install_service` (About view queries SCM and can re-run the bundled `eir-svc.exe install` elevated), and `check_updates_now` (About view's on-demand update check — installs and relaunches when a newer signed release exists; guarded by a shared `UPDATE_IN_PROGRESS` AtomicBool so it can't race the 6-hourly background checker into two concurrent installers) — UI-local.
- `util::gbp_per_usd` (USD→GBP rate via a hidden PowerShell `Invoke-RestMethod`, with a `0.79` offline fallback) and `util::open_url` (validates `http(s)://` then `Start-Process`) — UI-local helpers, not pipe traffic (`util.rs`).

Every service-facing command routes through the common async correlated sender: it checks the connection, adds a request id, and waits for the matching protocol-v2 `CommandResult`. Normal commands use 30 seconds; provider testing allows 135 seconds around the service's 120-second provider deadline. Applied, rejected, disconnected, and timed-out outcomes therefore reach the caller. During UI/service version skew, a protocol-v1 service receives the original flattened command and the UI returns the neutral result “Queued — this service cannot confirm application.” Long operations such as scans and updater cycles confirm that work started, then publish completion through status/activity. `get_status`, the autostart commands, and `util::*` are UI-local and do not use the pipe.

### Tray (`main.rs:114-323`)

- Tray icon is built from an embedded 128px PNG (`ICON_PNG`), recoloured per status (`status_accent` maps states to RGBA tints: green=Active/untouched, amber=Warning, orange=PendingApproval, blue=Executing, red=Error/ServiceDisconnected, grey=other) and Lanczos3-downsampled to 32px (`make_icon`, `recolor`, `decode_icon`).
- Start-with-Windows is registered by `tauri-plugin-autostart` with the app name `Eir` and the `--hidden` argument. The UI preference is stored in Tauri's app config directory as `ui-preferences.json`, defaults to enabled on first run, and is synced to the OS startup entry during Tauri setup. This separate preference prevents a user-disabled startup entry from being re-enabled on the next manual launch.
- A background task polls the cached status every 500ms and repaints the tray icon/tooltip only when `status` changes, and relabels the menu's pause entry only when `paused` changes (v0.24.1). Since v0.24.4 the change-guards advance only when the tray write succeeds, so a transient `set_icon`/`set_tooltip`/`set_text` failure retries next tick instead of freezing the tray on a stale state. The tooltip runs the status through `friendly_status()` so `"PendingApproval"` shows as `"Pending Approval"` (spaces each CamelCase boundary; unit-tested).
- Tray menu: Open Status / Pause Monitoring / Quit. The pause entry reads "Resume Monitoring" while paused (v0.24.1) and sends `UiMsg::TogglePause` via the cloned `UiCmdTx`; Open Status and left-click `unminimize()` + `show()` + `set_focus()` the window so a minimized window is restored, not just a hidden one (v0.24.1).
- **Close-to-tray**: `WindowEvent::CloseRequested` hides the window and `prevent_close()`s; the service keeps running; Quit fully exits (`main.rs:327-334`). The JS also redundantly intercepts `onCloseRequested` (`main.js:54-57`).
- Self-update: `tauri-plugin-updater` checks 15s after launch then every 6h, and on a newer signed release downloads/installs (NSIS, which elevates and updates the service too) and relaunches (`main.rs:203-232`).

### Layout, theming & rendering (`ui/index.html`, `ui/main.js`)

The frontend was fully rebuilt in v0.17 (still hand-written vanilla HTML/CSS/JS, committed, no build step):

- **Sidebar navigation** replaces the single card stack: Dashboard, Approvals (with a pending-count badge), Activity, App Updates, Learned, Settings, About — each a `.view` section toggled client-side. The sidebar footer holds the theme cycler and Pause/Resume.
- **Dashboard** shows a status hero (colour-keyed to the service state, with a plain-English headline from `STATUS_META` and the error line), CPU/memory/disk gauge cards, a pending-approvals call-to-action, "What the agent is thinking" (analysis + advisor badges), AI usage, and failed-services chips.
- **Theming**: dark / light / **system** (follows `prefers-color-scheme` live), persisted in `localStorage`, applied via `data-theme` on `<html>` with CSS-variable palettes.
- **Native feel**: the webview context menu is suppressed globally (`contextmenu` → `preventDefault`), chrome is `user-select: none` with content opted back in, and there are no native browser dialogs.
- **About view**: version from `get_app_version`; service version from `get_service_version` via the cached pipe status; SCM state from `get_service_state`, with an `install_service` button offered when the service is not installed; GitHub link via `util::open_url`; and an on-demand `check_updates_now`.
- **Poll cadence**: `refresh()` calls `get_status` and re-renders on `setInterval(..., 2000)` — a 2s poll of the *local cache* (no pipe round-trip per poll). `gbp_per_usd` is fetched once at load.
- **Render churn guards**: every list/text renderer that holds user-selectable content only mutates the DOM when its data changed — list renderers (`renderApprovals`, `renderActivity`, `renderLearned`, `renderUpdater`, `renderAsk`, `renderDisk`, `renderStartup`, `renderUsage`) compare a JSON **signature** and return early when unchanged; single text nodes use the `setText(el, v)` helper (writes only on change). Without this the 2s poll wipes an in-progress text selection and kills tooltips. Because the guards freeze the built HTML, relative ages are emitted as `<span data-ts="…">` and a single sweep at the end of `refreshInner` (`document.querySelectorAll('[data-ts]')`) re-ticks them each poll, so an approval that has sat for hours stops reading "just now" (v0.24.1). `ago(0)` → `''` keeps a zero timestamp blank.
- **Disconnected state**: `refreshInner` toggles a `svc-down` body class for the `ServiceDisconnected`/`Connecting`/`Restarting` states; CSS then greys and blocks pointer events on action buttons (approve/reject, scans, update-now, pause, per-row toggles) and dims the stale metric/history cards, so a click can't be silently dropped. The Rust `ensure_connected` gate is the authoritative backstop (v0.24.1).
- **XSS hygiene**: all service-supplied strings go through `esc()` / `escAttr()` before insertion into `innerHTML`; applied consistently across approvals, activity, updater rows, and service chips.
- **Activity feed** merges `recent_problems` + `recent_executions` into one list sorted by `at` descending, with emoji/tag per kind (`activityItems`).
- **v0.24.3 UX pass**: Escape hides the window (blurs first when a field has focus); Ask Eir sends on plain Enter (Shift+Enter = newline, IME composition guarded); every `data-ts` relative age gets an absolute local-time tooltip (set once per element); the sidebar Pause button turns amber with a ▶ icon while paused; a config-shaped `status.error` (provider/key/model/config) surfaces an "Open Settings" quick link in the hero; the Disk/Startup card headers summarise the last scan ("2.1 GB cleanable" / "12 entries, 3 disabled"); the "What the agent is thinking" meta line shows "analysed Xm ago" from the new `last_analysis_at` wire field; and numeric settings inputs are clamped to their declared min/max on save (`numVal`).
- **v0.25.1 UX pass** (frontend-only, no wire/service change): **toasts** (`toast(msg, kind)` → `#toast-wrap`, `textContent` only, auto-dismiss 2.6 s / 5 s for errors, click-to-dismiss, stack capped at 4) originally gave the then-fire-and-forget commands a neutral acknowledgement. Protocol v2 supersedes that constraint: current toasts use the returned command outcome, while neutral “queued” wording remains only for an older service during version skew. **Copy-to-clipboard** (`copyText`, `navigator.clipboard` with a textarea/`execCommand` fallback) on the hero error and per-approval target+details supports pasting a diagnosis elsewhere. **Activity filter chips** (All / Fixes / Diagnoses / Failures): `activityItems` tags each row `type` (`fix`/`diag`) + `fail`; `renderActivity` filters via `matchesActivityFilter`, folds `activityFilter` into the signature so a chip switch forces one rebuild, and keeps undo bookkeeping keyed on the *unfiltered* list so a hidden row cannot drop its in-flight Undo.
- **Settings** is a full view (no modal) populated from `lastStatus.settings`/`.updater.settings`/`.advisor.settings` plus the UI-local autostart command. Since v0.24.3 the view guards unsaved edits; v0.24.4 made the tracking **per-card** (`dirtyCards` set keyed on the four card ids `card-autostart`/`card-provider`/`card-advisor`/`card-updater`): any input marks its enclosing card dirty, a successful save clears only that card, a fill clears all, and re-entering the view refills only when no card is dirty — so saving the advisor card no longer discards unsaved edits still sitting in the provider card. The grouped provider select offers OpenRouter / Anthropic API / Claude CLI / Codex CLI / Kilo CLI and hides fields irrelevant to the selected provider. All three model controls use one native filterable `datalist`; `list_provider_models` reads OpenRouter's live catalogue, `codex debug models`, or `kilo models`, with static Claude and offline fallbacks. A request sequence rejects stale provider-A results after a switch to B, while per-provider in-memory selections and persisted-but-vanished ids remain visible. Changing provider clears incompatible model controls; the service also clears the old advisor escalation model at the authoritative config boundary. JS pre-validates key/model requirements before sending (`AiClient::new` remains authoritative). Provider/model settings apply live; only collector channels/poll intervals/log directories trigger the restart helper.

### Clear / Approve / Ignore / Update-now flows

- **Approve / Reject**: a delegated click handler on `#approvals` parses the card's `data-id`, **disables both buttons** to prevent double-submit, and calls `decide_approval(id, approved)`; on error it re-enables them (`main.js:271-280, 456-462`). Since v0.24.3, approving an **irreversible** action (`!info.reversible`, marked `data-irreversible` on the button) takes two clicks: the first arms the button (orange, "Click again to confirm — cannot be undone") for 6 s, the second submits; the armed state survives the 2 s poll because the approvals signature guard skips rebuilds, and a list rebuild simply disarms it. The command → `UiMsg::Approve` → pipe → decision loop, which resolves it against the persistent queue. The card disappears on the next poll once the service drops it from `pending_approvals`.
- **Pause**: header button (and tray menu) → `toggle_pause` → `UiMsg::TogglePause`; the button label flips Pause/Resume based on `status.paused` (`main.js:228-229, 266-269`).
- **Clear (Activity)**: one button fires both `clear_problems` and `clear_executions` then `refresh()`s (`main.js:601-604`). **Clear (Updates)** → `clear_update_history` (clears displayed attempts and learned update facts, but preserves scheduler timestamps so it cannot trigger an update).
- **Ignore / AI guidance (per app)**: Ignore sends `set_app_ignore { id, ignore, note:"" }`; blank remains **"unchanged"** so a toggle cannot wipe guidance. “Guide AI” sends `set_app_note`, and the saved-guidance list provides full create/read/update/delete access even after an ignored app leaves the latest results.
- **Learned fact override**: delegated click on `#learned-list` sends `set_learned_fact { id, op }`; the service updates the persisted fact and refreshes the broadcast list (`main.js:511-519`, `main.rs:930-934`).
- **Update now**: `#upd-now` → `run_updates_now` → `UiMsg::RunUpdatesNow`. The button is disabled when the updater is `running` or `!enabled`, because "the service ignores a manual run unless the updater is enabled" — the UI mirrors that gate rather than enforcing it (`main.js:369-375, 412-415`).

## Service decision loop, state & off-loop executor

The heart of `eir-svc` is a single async supervisory loop in `eir-svc/src/main.rs` that owns all mutable service state, collects signals, gates AI-proposed fixes through policy, and dispatches both the AI analysis call and actual fix execution to separate off-loop tasks so the loop never blocks. All paths cite `eir-svc/src/main.rs`.

### Process entry & service boilerplate

`main()` (lines 141-160) branches on `argv[1]`: `install`/`uninstall` manage the SCM registration, anything else attempts SCM dispatch via `service_dispatcher::start(SERVICE_NAME, ffi_service_main)`; if that fails (dev/standalone run) it builds a multi-thread Tokio runtime and calls `eir_main` with a Ctrl-C shutdown future.

- `define_windows_service!(ffi_service_main, svc_main)` (line 42) wires the SCM entry point.
- `run_service()` (lines 50-102) registers an event handler that, on `Stop`/`Shutdown`, sets a shared `AtomicBool`; reports `ServiceState::Running` (accepting STOP|SHUTDOWN); builds the runtime; and `block_on(eir_main(...))` where the shutdown future polls the atomic every 500 ms (lines 81-89). After the loop returns it reports `ServiceState::Stopped`. `Interrogate` returns `NoError`; other controls return `NotImplemented`.
- `install_service()` (104-128) creates the service as `OWN_PROCESS`, `AutoStart`, `account_name: None` (i.e. **LocalSystem**). `uninstall_service()` (130-139) stops then deletes it.

### `SvcState` — the single owned state struct (lines 164-224)

Not shared/locked — it lives on the loop task and is mutated inline; the UI sees it only via broadcast snapshots. Fields:
- Live metrics: `paused`, `cpu`, `memory`, `disk`, `failed_services`.
- AI/diagnosis: `last_analysis`, `recent_problems: VecDeque<ProblemSummary>` (capped 20, FIFO via `push_problem`), `recent_executions: VecDeque<ExecutionSummary>` (capped 20 via `push_execution`).
- `pending: Vec<PendingApproval>` — actions awaiting user decision, **mirrored to the audit DB** so the queue survives restarts.
- UI surface: `status: String`, `error: Option<String>`, `usage`, `settings`.
- Updater: `updater: UpdaterStatus` (broadcast view), `updater_running: bool` (prevents overlapping cycles).
- `in_flight: HashSet<String>` — `format!("{action:?}")` labels currently queued/executing on the off-loop worker; used for dedupe and to reflect "Executing".
- Advisor: `advisor: Option<AdvisorStatus>`, `advisor_spent_today: f64`, `advisor_escalations_today: u32`, `advisor_spend_date: String` (UTC `YYYY-MM-DD` the counters belong to).

`build_status(&SvcState)` (241-259) projects `SvcState` into the wire `StatusPayload`, mapping `pending` to `info` clones and wrapping `updater` in `Some`. Every state change is followed by `pipe.broadcast_status(build_status(&st))`.

### `eir_main` startup (lines 565-728)

1. File-only tracing to `eir.log` next to the exe (a service has no console); the non-blocking guard is `mem::forget`-ed to live forever (lines 567-581).
2. `pipe_server::spawn()` returns the broadcast handle and `ui_rx` command channel; `SvcState::default()` created.
3. A `fatal!` macro (586-595) sets status `"Error"`, broadcasts, and `return`s — used for config/policy/DB init failures so a hard misconfig stops cleanly but still informs the UI.
4. Loads `config.toml` and `policy.toml`; the live `confidence_threshold` is overridden from config (`pol.execution.confidence_threshold = cfg.monitoring.confidence_threshold`, line 613) — policy.toml only supplies the fallback.
5. Inits SQLite (`audit::init_db`), cleans stale updater staging, seeds `updater`/`advisor` status from config + history, sets `advisor_spend_date` to today.
6. **AI client init is non-fatal** (646-656): a bad AI config sets `status="Error"` + an actionable error and leaves `ai = None`, but the service keeps running so Settings stays usable. The post-warmup settle (and its `resting_status` re-settle) is gated on `ai.is_some()` (v0.24.4) — it previously cleared this error before the first broadcast, leaving a mis-configured provider looking healthy forever.
7. Spawns signal collectors: `event_log`, `file_watch` (after `discover_watch_dirs`), `wmi`. Sleeps 5 s to let them warm up.
8. Restores `usage_summary` plus pending approvals from the DB. Durably claimed (`approved`) rows are replayed into the executor after its channel starts, so a service crash cannot lose an accepted action. Legacy active rows have their semantic action keys backfilled and duplicates removed before either queue is loaded.
9. Sets up the decision `ticker` (`cfg.monitoring.decision_interval_secs`), channels, and the executor worker (below).

### The `tokio::select!` decision loop (lines 729-1378)

The outer `tokio::select!` races the main loop future against the `shutdown` future. Inside the `loop {}`, an inner `tokio::select!` multiplexes the sources; command/outcome arms `continue` immediately so they are serviced without waiting for a decision tick:

1. `ticker.tick()` — falls through to the decision body below.
2. `trigger_rx` — a collector saw something actionable. The arm only *schedules* a reaction: `react_at = now + REACTIVE_DEBOUNCE (10s)`, floored to `last_cycle_at + REACTIVE_MIN_GAP (60s)`, then `continue`s. Scheduling a deadline (instead of sleeping inline) keeps every other arm responsive during the debounce, and a trigger landing inside the min-gap is **deferred, never dropped**.
3. `sleep_until(react_at)` (enabled only while a reaction is scheduled) — drains any coalesced pings and falls through to the decision body, exactly like a tick. The body clears `react_at` (a scheduled tick that fires first also cancels the pending reaction so the same signals aren't analysed twice).
4. `update_done_rx` — an update cycle finished: clears `updater_running`/`updater.running`, records `last_run`/`last_cost_usd`/`notes`/`apps`, sets `phase="idle"`, refreshes history, recomputes `next_run`.
5. `update_progress_rx` — coarse live phase label; **guarded on `updater_running`** so a straggling message can't overwrite the `idle` a just-finished cycle set.
6. `exec_done_rx` — a fix finished on the worker: removes its semantic key from `in_flight`, reloads the durable pending queue when a panic/timeout or rolled-back registry reset was requeued, pushes an execution + an `auto_executed=true` problem entry, and settles `resting_status`. It explicitly **does not touch `st.error`** so an execution outcome cannot wipe an unrelated AI/connection error.
7. `analysis_done_rx` — the off-loop AI analysis (and optional advisor escalation) finished: on `Err`, sets `status="Error"`/`error` and continues; on `Ok`, destructures the returned `AnalysisSuccess`, logs usage, updates `last_fingerprint`/`last_analysis_at`/`st.advisor*`, reloads `learned_facts` fresh (they may have changed during the multi-minute call), then runs `audit::log_decision` and the **same per-problem routing + tray-status logic previously inline in the per-cycle body** (see below).
8. `ui_rx` — UI commands (see below).

#### Per-cycle body (after a tick, lines ~1287-1525)

- `cycle_count += 1`; every 20 cycles re-discovers log dirs and feeds new ones to `file_watch` via `dir_update_tx`.
- **Scheduled updater**: if `enabled && !paused && !updater_running && (last_run==0 || elapsed >= interval)`, sets running flags, `phase="checking…"`, and `spawn_update_cycle`.
- `if st.paused { continue; }` — paused skips analysis but **still ran the updater gate above** (which itself checks `!paused`, so paused also suppresses updates).
- Collects `decision_history` (last 5), builds a `SignalSnapshot` (event log + file changes drain + wmi current), updates live metrics, broadcasts.
- `let Some(ai) = ai.as_ref() else { continue }` — no provider configured: keep collecting/serving UI, skip analysis.
- **`if analysis_running { continue; }`** — an off-loop analysis (below) is already in flight; resettle `resting_status`, clear error (unless paused), broadcast, and skip this pass entirely (including the feedback/label/idle-skip work below it), so a redundant tick or reaction never queues a second overlapping analysis.
- Updates feedback after-states and pulls `feedback::recent_summary`; dispatches `learn::label_one` **off-loop** (it makes an AI call, so running it inline could stall `ui_rx` for up to the provider timeout — a fire-and-forget task, guarded by an `AtomicBool` so it never stacks, with a drop-guard reset) and refreshes `learned_facts` for the UI card.
- **Idle-skip gate**: computes `actionable_fingerprint`; `changed = fingerprint.is_some() && != last_fingerprint`; `heartbeat_due` if `last_analysis_at` is `None` (forces baseline) or elapsed ≥ `ANALYSIS_HEARTBEAT` (6 h). If `!changed && !heartbeat_due`, settle `resting_status`, clear error (unless paused), broadcast, `continue` — saves AI spend on unchanged idle cycles.
- `learn::analyse_issues` + `LearnedFacts::load` build `learned_section` for the prompt, then **the AI call is dispatched off-loop** (see "Off-loop analysis task" below) and the per-cycle body `continue`s immediately — `ai.analyze(...)`, the optional advisor escalation, `audit::log_decision`, the per-problem routing, and the tray-status broadcast all now run in the `analysis_done_rx` arm once the task reports back, not inline here.

#### UI command handlers (`ui_rx`, lines 787-971)

- `TogglePause` → flip `paused`, resettle status.
- `ClearProblems` / `ClearExecutions` / `ClearAsk` → clear the respective deque / Ask history.
- `RefreshStatus` → force a fast services-only rescan (`wmi::rescan_failed_services`, no PowerShell), overwrite `failed_services`, re-settle `resting_status`, and broadcast — clears a recovered service on demand. Deliberately does **not** touch `st.error` (that is a live AI/config error, not stale status).
- `UpdateSettings` → `apply_update`, compute `settings_update_needs_restart` **before** mutating config, **validate by constructing a fresh `AiClient`**; on failure reject and reload config from disk (never apply a bricked provider); on success save config. If the update changed collector spawn parameters (event-log channels, event-log/WMI poll intervals, log directories), `restart_self()` then `return`; otherwise the new AI client, policy confidence threshold, and decision ticker are swapped live without a restart. If the restart helper fails to spawn, the service stays alive on the old settings and surfaces an error instead of stopping with nothing to start it again.
- `Approve { id, approved }` atomically claims a still-pending database row. An accepted row remains durable as `approved` until the executor transaction either completes it or requeues it; startup replays claimed rows after a crash. Current policy and target preflight are rechecked before execution. A rejection is recorded for learning and then retired. Always resettle status + broadcast.
- `RunUpdatesNow` → requires monitoring to be resumed and no cycle already running, but deliberately bypasses the updater's enabled/schedule gate so the user can request a one-off check.
- `ClearUpdateHistory`, `UpdateUpdaterSettings`, `SetAppIgnore`, `SetAdvisorSettings` → applied **live, no restart** (unlike provider settings), each persisting via `config::save`.

### Off-loop executor (lines 378-458)

To keep the loop responsive, fix execution is serialised on a dedicated worker:
- `ExecJob`: `action`, `decision_id`, proposal-time `baseline`, semantic dedup `key`, display `label`, diagnosis/confidence/reason, and the optional durable `approval_id`.
- `ExecOutcome`: the display/audit result, optional registry `undo_id`, optional cleared undo id, and a `refresh_pending` flag.
- `spawn_executor` consumes `ExecJob`s on an unbounded channel. Only `executor::execute` runs inside the timeout-bounded inner task, so a panic or timeout becomes a synthetic failed `ExecutionResult` while the worker survives. Every result then passes through `audit::persist_execution`, which atomically writes `execution_log`, marks the decision executed, persists any required registry undo, and deletes or requeues the approved row. Synthetic failures are logged and requeued instead of becoming hidden `approved` rows. A successful registry reset whose execution+undo transaction fails is immediately restored from its in-memory snapshot; only a successful rollback is logged/requeued. Feedback is written after that durable commit, then `ExecOutcome` is sent on `done_tx`.
- The loop sends jobs via `exec_tx` and folds outcomes via the `exec_done_rx` arm. Unbounded channels are used deliberately (job volume bounded by problems-per-cycle + approvals) so a send never blocks the loop.

### Off-loop analysis task (`AnalysisSuccess`, lines ~413-423, spawn ~1435-1525)

The AI call — `ai.analyze(...)` plus its optional advisor-escalation `ai.analyze_with(...)` — is the other multi-minute call on the loop (shells out to the Claude CLI or hits an HTTP API), so it's dispatched the same way as `spawn_update_cycle`, gated by a loop-local `analysis_running` flag (never overlaps a running analysis) instead of blocking `ui_rx` inline:
- `AnalysisSuccess` carries everything the receiving arm needs back from the task: `snapshot`, `fingerprint`, `decision`, `usage: Vec<CallUsage>`, `advisor: AdvisorStatus`, `advisor_spent_today`, `advisor_escalations_today`. The channel carries `Result<AnalysisSuccess, String>` directly — no separate outcome wrapper.
- The spawned task mirrors `spawn_update_cycle`'s hardening: the real work runs in a nested inner `tokio::spawn` under a `tokio::time::timeout(ANALYSIS_MAX = 10 min, …)`, so a panic (`JoinError`) or a hang still sends a `Result::Err` and releases `analysis_running` — an analysis can never latch "running" forever.
- Inside the task: `ai.analyze(...)`; if `should_escalate` returns `Some(reason)`, re-analyses via `ai.analyze_with(model, effort)` and folds the escalation's usage/decision/`AdvisorStatus` in — the same advisor logic as before, just running off-loop.
- `analyze_with` wraps each provider call in a bounded transient-retry loop (`MAX_AI_RETRIES = 2`, 2s/4s backoff) gated by `is_transient_ai_error` (HTTP 429/5xx/overload/timeout/connection markers). A CLI subprocess (Claude/Codex/Kilo) that exits non-zero with **empty stderr** is also transient: each CLI adapter emits a distinct "…and no error output (transient)" message so a swallowed API/overload blip retries instead of latching `st.error` red, while an exit that *did* print a diagnostic (auth/config) still fails immediately.
- The `analysis_done_rx` arm (see the select-arm list above) is where `audit::log_decision`, the **per-problem routing**, and the tray-status broadcast now live, unchanged from before the move: `parse_fix_action()` (unparseable → blocked problem), then `pol.evaluate(&action, confidence)` → `Block(reason)` pushes a blocked problem; `AutoApprove` checks `safety::rate_limited` + `in_flight` dedupe and sends an `ExecJob` (reason `None`); `RequireApproval(reason)` dedupes against `pending`/`in_flight`, builds `ApprovalInfo`, and `insert_pending_approval`s. Finally computes the tray status (`Paused > PendingApproval > Executing > Warning(problems_found) > Active`) and broadcasts.

### `actionable_fingerprint` (lines 465-525)

Returns `Some(stable_string)` only when something is worth analysing, else `None` (idle, skips the AI call). It collects sorted parts from:
- File-change log events where `severity != "INFO"` or there are error snippets (benign INFO ignored).
- Windows events with level `Error` or `Warning`.
- Each failed service; resource thresholds `CPU>90`/`MEM>90`/`DISK>90`.
- **Security gating**: firewall profiles only when `Some(false)` (off) — `None` (unknown) and `Some(true)` (on) are ignored, so a secure box stays idle. Defender faults (`realtime_off`, `sig_stale` if `signature_age_days > 3`) count **only when Defender is the active AV** (`antivirus_enabled != Some(false)`) — a third-party AV's passive Defender is treated as normal.

Identical fingerprints across cycles mean nothing changed → skipped until the 6 h heartbeat.

### `resting_status` & `should_escalate` & `restart_self`

- `resting_status` (363-374): the non-cycle status with precedence **Paused > PendingApproval > Executing > Active**. Asserted by `status_tests::resting_status_precedence`.
- `should_escalate` (271-302): pure. Returns `Some(reason)` only when advisor `enabled`, a deeper tier is configured (`escalation_model` or `escalation_effort` non-empty), `escalations_today < MAX_ESCALATIONS_PER_DAY` (=24, the provider-agnostic backstop), and either `needs_deeper_analysis` ("the agent flagged the signals as ambiguous") or non-empty problems whose max confidence < `low_confidence_threshold` ("confidence was low"). Spend is retained for visibility, not gating. Covered by `advisor_tests`.
- `restart_self`: spawns a detached PowerShell helper (LocalSystem, no UAC, survives this process exiting) that waits for EirSvc to stop cleanly, then retries `sc start` every 5s over a 60s window checking for Running each second; all helper streams redirect to `eir-restart.log`. The caller flushes the correlated settings result to the UI before returning and reporting STOPPED. A helper spawn failure keeps the service alive instead of stopping into the void.
- `spawn_update_cycle` (307-359): runs a cycle on a detached task with a nested inner `tokio::spawn` + `CYCLE_MAX = 60 min` timeout watchdog, so a panic (JoinError) or hang still produces a `CycleSummary` and releases `updater_running` — the updater can never latch "running" forever.

## Signal sources

Eir's signal layer is three independent background collectors that each maintain their own in-memory buffer, plus a per-cycle aggregation step in the decision loop that drains them into one `SignalSnapshot`. Each collector runs on its own cadence and writes to a shared, lock-guarded buffer; the decision loop reads a consistent slice of all three on each tick. All collectors live under `eir-svc/src/signals/` (`mod.rs` is just the module list — `event_log`, `file_watch`, `log_parser`, `wmi`).

### Data model (`eir-svc/src/models.rs`)

- **`SignalSnapshot`** (lines 5–12) is the per-cycle bundle handed to the AI: `timestamp`, `event_log: Vec<EventLogEntry>`, `file_changes: Vec<FileChange>`, `system_state: SystemState`, `decision_history: Vec<PastDecision>`. The first three come from the three collectors; `decision_history` is loaded separately from the audit DB (`audit::get_recent_decisions(&db, 5)`, `main.rs:1022`), not from a signal source.
- **`SystemState`** (lines 50–68): `uptime_secs`, CPU/memory/system-drive metrics, service state, network interfaces/errors, disk health, Windows Update status, security posture, plus `collected_at` and `collector_errors`. Network errors and disk health are best-effort measurements; a failed probe retains the last-good value and names the source error instead of publishing a healthy zero.
- **`SecurityPosture`** (lines 72–76) = `FirewallStatus` + `DefenderStatus`, both `Default`. `FirewallStatus` (80–85) holds `domain/private/public: Option<bool>` (`true`=on, `None`=unreadable, deliberately not a fault). `DefenderStatus` (90–98): `realtime_enabled`, `antivirus_enabled`, `signature_age_days`, all `Option`, `None` when Defender is absent or the query fails.
- **`EventLogEntry`** (14–21): `timestamp`, `level`, `source`, `message`, `event_id`. **`FileChange`** (23–31): `path`, `kind`, `size_bytes`, `timestamp`, `log_event: Option<LogEvent>`. **`LogEvent`** (34–48): `program`, `log_path`, `severity` (FATAL/ERROR/WARN/INFO), `error_snippets` (≤5), `content_excerpt` (capped raw text so the AI can disambiguate a benign `"error"` JSON field from real corruption).

### Source 1 — Windows Event Log (`signals/event_log.rs`)

- **Collects:** Error/Warning/Information records from configured channels (default `["System", "Application"]`, `config.rs:282`) via the legacy Win32 EventLog API (`OpenEventLogW`/`ReadEventLogW` reading `SEQUENTIAL_BACKWARDS` = newest-first, line 16).
- **Cadence:** `event_log_poll_interval_secs`, default 45 (struct default `config.rs:280`; embedded sample uses 30; clamped to a 5 s floor on update, `config.rs:210`).
- **Cursor logic:** per-channel `HashMap<String,u32>` of the highest `RecordNumber` delivered (lines 149, 160–164). First poll primes from 0; subsequent polls return only records with `RecordNumber > last` (lines 90–94). Newest-first read stops at the first already-seen record.
- **Bounding & delivery:** `MAX_PER_POLL = 100` caps entries **per channel** — but the cursor still advances to the true newest, so the freshest events are always kept and nothing below the cursor is re-read. Each poll **appends** into a shared rolling buffer capped at `BUFFER_CAP = 100`, pushed oldest-first so `pop_front` eviction drops the **oldest** (keeping the freshest errors); the decision loop **drains** it (`event_log::drain`) — one-shot delivery, like the file-watch buffer. (The former `RING_SIZE = 20` per-poll-across-all-channels cap silently and *permanently* dropped every record past the 20th in a burst and starved later channels — both fixed.)
- **Reactive trigger:** a poll containing a fresh **Error**-level entry pings the decision-loop trigger channel. Warnings deliberately don't trigger (Windows emits them near-continuously; the scheduled sweep still analyses them) — reacting to each would burn AI calls on noise.
- **Message extraction:** insertion strings embedded in each event record are decoded, sanitised, and bounded into the message beside the low-16-bit Event ID. This recovers the actionable detail Windows stores without loading arbitrary provider DLLs. Blocking Win32 work stays in `spawn_blocking`.

### Source 2 — File / log watcher (`signals/file_watch.rs` + `signals/log_parser.rs`)

- **Discovery (`discover_watch_dirs`):** scans fixed roots (`C:\Windows\Logs`, `C:\Windows\Temp`, `C:\Temp`, `C:\Logs`) plus active-user roots (`LOCALAPPDATA`, `APPDATA`, `TEMP`, `TMP`) and `PROGRAMDATA`. A root/subdir is watched only if it contains a recognised recent text file; configured extras are always included when present. Discovery and all subsequent watcher/file reads run while impersonating the active desktop user, so LocalSystem never traverses user-controlled reparse points with SYSTEM authority. Re-discovery feeds new dirs to the live watcher.
- **Watching:** `notify` `RecommendedWatcher`, `RecursiveMode::Recursive`, on a dedicated OS thread. It reacts to `Create`/`Modify` only, re-arms known directories after recreation, and stays alive when startup discovery is empty so a later login can add roots.
- **Per-event parse:** for each changed path it reads `size_bytes` and calls `try_parse_log` (lines 200–209). `try_parse_log` (27–42) skips empty files, requires one of `TEXT_EXTENSIONS` (log/txt/csv/json/xml/ini/cfg/conf/err/out/trace/debug/warn/error/info, lines 14–17), reads at most the **last** `MAX_READ_BYTES = 65_536` of the file (`read_tail`, dropping the partial first line) so a rolling log that has grown past 64 KB still has its newest lines parsed, and runs `log_parser::parse`. A result that is INFO with no error snippets is dropped to `None` (line 37) — only "interesting" log events attach to the `FileChange`.
- **`log_parser::parse`** (`log_parser.rs:38–48): infers `program` from path shape (Program Files / ProgramData / Windows\Logs\<Subsystem> / AppData\(Local|Roaming|LocalLow), lines 64–125, falling back to parent dir name); `extract_errors` (129–171) walks lines, classifies against `ERROR_KEYWORDS`/`WARN_KEYWORDS` (lines 4–30), raises a severity ceiling (FATAL > ERROR > WARN > INFO), and collects up to 5 non-overlapping snippets (1 line before + 2 after, lines 161–167); `excerpt` caps raw content at `MAX_EXCERPT_CHARS = 2500` with a truncation marker (lines 35, 52–60).
- **Bounding:** `RING_SIZE = 50`, a true rolling ring buffer (`pop_front` when full). File changes are **drained**, so each `FileChange` is delivered to the AI at most once.
- **Reactive trigger:** a change whose parsed `LogEvent::is_actionable()` (severity ≠ INFO, or error snippets present — the shared predicate in `models.rs`) pings the decision-loop trigger channel from the watcher thread (`try_send`, never blocks).

### Source 3 — System state / WMI (`signals/wmi.rs`)

- **Cadence/freshness:** `wmi_poll_interval_secs`, default 300, with a 30 s floor. `snapshot_state` runs in `spawn_blocking`; the cache records its collection timestamp and per-probe error tokens. A failed probe keeps the last known good value but marks that collector degraded; before the first collection, metrics are explicitly `not_collected` and the UI renders `—`, never a healthy zero. A failed manual service rescan preserves the prior service state.
- **What it collects** (mostly direct Win32, not WMI, despite the name):
  - Uptime: `GetTickCount64` (lines 31–33).
  - Memory: `GlobalMemoryStatusEx` → load % + available GB (83–94).
  - Disk: `GetDiskFreeSpaceExW` on `C:\` → usage % + free GB (96–115).
  - Services: `OpenSCManagerW` + `EnumServicesStatusExW` (two-pass: size then read) over active Win32 services; counts running and lists names not in `SERVICE_RUNNING` as `failed_services` (117–199).
  - Network interfaces: `GetAdaptersInfo`, marking each up/down by whether it has a non-`0.0.0.0` IPv4 (207–242).
  - Windows Update: registry read of `…\WindowsUpdate\Auto Update\Results\Install\LastSuccessTime` → `"last_install: <time>"` or `"unknown"` (244–289).
  - CPU: the **only real WMI call** — `Get-WmiObject Win32_Processor … LoadPercentage` via PowerShell (74–81).
- **`ps_capped` (the bounded probe):** spawns PowerShell non-interactively, captures stdout/stderr, rejects non-zero exit codes, and kills on deadline. CPU and Defender each have a 15 s cap so one wedged provider cannot stall the whole snapshot; failure is surfaced through `signal_errors` while the last good measurement remains visible.
- **GPO-aware firewall (`get_firewall` + `effective_firewall`, lines 330–362):** reads `EnableFirewall` REG_DWORD from both the local store (`…SharedAccess\Parameters\FirewallPolicy\{DomainProfile|StandardProfile|PublicProfile}`) and the GPO store (`…Policies\Microsoft\WindowsFirewall\{DomainProfile|PrivateProfile|PublicProfile}`). Note the naming mismatch: local calls private "StandardProfile", GPO calls it "PrivateProfile" (handled at lines 358–360). `effective_firewall` (330–336): policy ON → `Some(true)`; **policy OFF → `None`** (a GPO is deliberately holding it off and `netsh` can't override, so Eir treats it as "not ours to fix," preventing a futile `firewall_enable` loop on managed machines); no policy → the local value. Unreadable stays `None` so the AI never reads "couldn't read it" as "firewall off." `read_reg_dword` (294–321) checks the value **type** is `REG_DWORD` and length is 4, rejecting a 4-byte string/binary value rather than misreading it.
- **Defender parse (`get_defender` + `parse_defender_status`, lines 364–395):** one `ps_capped` call runs `Get-MpComputerStatus -ErrorAction SilentlyContinue` and formats `'{0}|{1}|{2}'` from `RealTimeProtectionEnabled|AntivirusEnabled|AntivirusSignatureAge`. `parse_defender_status` splits on `|`; each field parses independently to `Some`/`None` (bools tolerant of casing/whitespace, age as `u32`), so any garbage/empty field degrades to `None` instead of failing the snapshot. Absent Defender / timeout → empty output → all `None`.
- Unit tests in-file cover the firewall GPO matrix and Defender parsing (lines 484–529).

### Aggregation and the actionable fingerprint (`eir-svc/src/main.rs`)

- **Wiring:** all three spawn at startup (`event_log::spawn`, `file_watch::spawn` after `discover_watch_dirs`, `wmi::spawn`), each holding a clone of the reactive `TriggerTx` (`signals/mod.rs` — capacity-1 tokio mpsc, `try_send` so a burst coalesces and a send never blocks); a 5 s settle sleep follows before status flips to "Active".
- **WMI trigger:** each snapshot computes `fault_key` from the shared `SystemState::fault_parts()` (failed services, firewall explicitly off, Defender faults while Defender is the active AV) and pings the trigger whenever the key **changed** — including a fault *clearing* (key → empty), so a recovered service is reflected within one reactive debounce instead of waiting for the next scheduled decision tick. An unchanged key never re-triggers, so a persistent fault doesn't fire every poll. A pure recovery leaves `actionable_fingerprint` empty, so the idle-skip gate suppresses the expensive AI **analysis** — the wake just refreshes the UI (clears the stale failed-service chip). It reaches the loop body like any other reactive wake, so the bounded once-per-fact learned-fact labeller may still run if a fact is unlabelled (unchanged from prior behaviour); steady-state that's zero.
- **Per cycle:** the decision loop builds the snapshot from `event_log::drain` and `file_watch::drain` (both one-shot reads), `wmi::current` (clone of latest), plus DB decision history.
- **`actionable_fingerprint`** decides whether a cycle is even worth an AI call and dedups unchanged states. It is built from **shared predicates in `models.rs`** so the fingerprint and the reactive triggers can't drift: `LogEvent::is_actionable()` (`F|path|sev|count`), `EventLogEntry::is_actionable()` (`E|level|source|id`), and `SystemState::fault_parts()` (`S|name`, `FW|name` only when explicitly `Some(false)`, `DEF|realtime_off`/`DEF|sig_stale` for age `>3` only when Defender is the active AV), plus CPU/MEM/DISK `>90` flags. Parts are sorted and joined; **empty → `None` → skip the Claude call.** Identical fingerprint across cycles means nothing changed, so it's skipped.

## AI layer & prompts

The AI layer lives in `eir-svc/src/ai/` (`mod.rs` re-exports `client` and `prompt`). It turns a `SignalSnapshot` into a structured `ClaudeDecision` (analysis + ranked problems + proposed fix actions), behind a provider abstraction that covers five backends. The monitoring loop in `eir-svc/src/main.rs` drives it, layers advisor-mode escalation on top, and records token/cost usage. By the codebase's layering convention this is an infrastructure adapter (concrete HTTP/subprocess clients) plus a pure prompt-builder; the domain types it produces (`ClaudeDecision`, `Problem`, `FixAction`) live in `eir-svc/src/models.rs`.

### Provider abstraction (`ai/client.rs`)

`AiClient` (`ai/client.rs`) wraps a single `reqwest::Client` (300s timeout, set for slow free OpenRouter models), the normalised reasoning `effort` string, and an internal `enum AiClientConfig` with one variant per provider: two native HTTP backends (`Anthropic`, `OpenRouter`) and three subprocess backends (`ClaudeCli`, `CodexCli`, `KiloCli`) that borrow a locally logged-in subscription session instead of taking a pasted key (only the OpenAI-compatible proxy stays removed — its legacy config value aliases to Anthropic on load). **v0.19 removed the API-key-based `KiloCode` gateway provider** (`api.kilo.ai`) in favour of `KiloCli`; a `provider = "kilocode"` (or legacy `"kilo"`) in an old config loads as `kilo_cli`:

All three subscription CLIs share one privilege boundary: when EirSvc is LocalSystem, it obtains the active console user's primary token with `WTSQueryUserToken` and starts a hidden process with `CreateProcessAsUserW`. Scratch-file redirection replaces standard-handle inheritance, which Windows forbids across sessions. User-owned CLI binaries therefore never execute as SYSTEM.

- **`Anthropic`** — native `/v1/messages`, streaming SSE, `x-api-key` + `anthropic-version: 2023-06-01`. Requires `anthropic_api_key` and a non-empty model or `new()` bails. Sends `output_config.effort` when an effort is configured — except for Haiku models, which do not support the dial. Parses per-call usage from the stream (`message_start` input/cache tokens, `message_delta` output tokens) and **estimates** cost from a small list-price table (`anthropic_price_per_mtok` / `estimate_anthropic_cost` — prices drift) for the usage card and advisor spend visibility.
- **`OpenRouter`** — OpenAI-compatible streaming against `https://openrouter.ai/api/v1`. Key comes from config or is auto-detected from `~/.openrouter/config.json` under any `C:\Users` profile (`resolve_openrouter_key`); blank model defaults to `"openrouter/free"`. Adds `HTTP-Referer`/`X-Title` attribution and requests a final usage chunk (tokens + provider-reported cost). Effort maps to `reasoning.effort` (`xhigh`/`max` collapse to `high`).
- **`ClaudeCli`** — Claude on the user's **subscription**: spawns the local `claude` binary (`--print --output-format json`, optional `--model`, `--effort`), **no API key**. `resolve_user_profile` uses the configured profile or scans `C:\Users\*\.claude\.credentials.json`; `resolve_claude_binary` tries the configured path, then `<profile>\.local\bin\claude.exe` (native installer), then npm's install layout under the profile — `AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe` (the exe the `claude.cmd` shim dispatches to), then the `claude.cmd` shim itself (npm's stable entry point, covering package-layout drift) — then PATH. Usage comes from the CLI's JSON envelope (`result`, `total_cost_usd`, cache-token fields — the cost is the *equivalent* API cost; the call itself is covered by the subscription, and the UI shows "—"/no-charge). Blank model = the CLI's configured default.
- **`CodexCli`** — Codex on the active desktop user's **ChatGPT subscription**, **no API key**. `resolve_codex_binary` tries the configured override, OpenAI desktop install, Codex standalone package, npm native layouts/shim, then PATH. Each call gets a fresh empty scratch cwd and runs `codex [--search] --ask-for-approval never exec --json --sandbox read-only --skip-git-repo-check --ephemeral --ignore-user-config --ignore-rules --color never`, with the prompt on stdin. `-m` selects a model; reasoning uses `-c model_reasoning_effort=…` (`max` clamps to `xhigh` before GPT-5.6). `parse_codex_ndjson` takes final `agent_message` text and `turn.completed` usage, subtracting cached input from the base input bucket to avoid double-counting; subscription cost is unknown/zero. Only the update-check path adds global `--search`.
- **`KiloCli`** — Kilo Code on the user's **subscription** (Kilo Pass / Token-Plan addons / BYOK): spawns the local `kilo` binary (`run --auto --format json --agent ask`, optional `-m`/`--variant`), **no API key**. `resolve_kilo_cli_profile` scans `C:\Users\*\.local\share\kilo\auth.json` — the CLI's actual session file, a JSON object keyed by provider name (`"kilo"` for the Pass/Token-Plan login, plus any BYOK provider entries). `resolve_kilo_cli_binary` mirrors `resolve_claude_binary`'s structure but for npm's install layout: it tries the configured path, then the platform-specific exe under the resolved profile's `AppData\Roaming\npm\node_modules\@kilocode\cli\node_modules\@kilocode\{cli-windows-x64,cli-windows-x64-baseline}\bin\kilo.exe` (Windows npm installs resolve to either variant depending on CPU feature detection at install time), then the npm shim `kilo.cmd`, then falls back to bare `"kilo"` on PATH. Output is NDJSON (`parse_kilo_ndjson`): each event nests its real payload under a `part` object (e.g. `{"type":"text","part":{"text":"..."}}`) — `text` events concatenate into the reply and the last `step_finish` event's `part.tokens`/`part.cost` become usage, with a fallback to the equivalent top-level fields for older/alternate event shapes. **Models routed through the subscription/Token-Plan/BYOK require a `kilo/` prefix** (e.g. `kilo/minimax/minimax-m3`); a bare `provider/model` id (e.g. `minimax/minimax-m3`) instead routes to that provider directly and fails without the user's own credentials there — Eir does not auto-prefix, a wrong model string surfaces the CLI's own error.

Both SSE readers share `SseLineBuf`, a byte-based line accumulator (unit-tested), so a multi-byte UTF-8 character split across network chunks can never drop data. CLI envelope/NDJSON decoding is unit-tested for Claude, Codex, and Kilo.

`AiClient::new` is the construction seam; it's rebuilt per update cycle from the live `ApiConfig` (`spawn_update_cycle`).

### Analysis entry points & response parsing

`analyze` delegates to `analyze_with` with no overrides. `analyze_with` is the single dispatch point:
1. Builds the prompt via `prompt::build`.
2. Applies a `model_override` and an `effort_override` to **every** provider (both trimmed and ignored if empty). This is the advisor escalation lever.
3. Dispatches to the per-provider call, returning raw text + `Option<CallUsage>` (every provider now reports usage where available; Anthropic's cost is estimated, OpenRouter's is provider-reported, the Kilo CLI's step_finish cost is provider-reported where present).
4. Parses: `strip_fences` removes ```` ```json ````/`~~~` fences, then `serde_json::from_str::<ClaudeDecision>`; on failure it falls back to `extract_json_object` (first `{` … last `}`) to handle reasoning models that wrap JSON in prose. A hard parse failure propagates an error with the raw text attached.

Each streaming path surfaces mid-stream provider errors instead of returning empty: OpenAI-style bails on a streamed `error` object and on an empty final body (`client.rs:445-448`, `469-471`); Anthropic only accumulates `content_block_delta`/`text_delta` events.

### The monitoring prompt (`ai/prompt.rs`)

`prompt::build` (`prompt.rs:3`) assembles one large user-message string. Injected context, in order:
- **LOG EVENTS section** (`format_log_events`, `prompt.rs:194-233`): for every `FileChange` whose `log_event` has non-empty `error_snippets`, it emits program / file path / severity / error snippets / a raw `content_excerpt` ("judge the finding in context"). Flagged "highest priority for diagnosis".
- **CURRENT SYSTEM STATE**: the full `SignalSnapshot` serialized to pretty JSON, **with `decision_history` stripped out** (`prompt.rs:9-12`) since history is rendered separately.
- **RECENT DECISION HISTORY (last 5)**: the `history: &[PastDecision]` arg (loaded via `audit::get_recent_decisions(&db, 5)`, `main.rs:1022-1023`), pretty JSON.
- **RECENT EXECUTION FEEDBACK** (`prompt.rs:17-26`): only when `feedback_summary` is `Some`, non-empty, and not the sentinel `"No execution history yet."`. Built by `feedback::recent_summary(&db, 10)` (`feedback/mod.rs:112`), which joins `execution_feedback` to `execution_log`. Each line is `- <ts>: <action> -> SUCCESS|FAILURE<delta><[reason: ...]>`; FAILUREs carry the condensed error text so the model can avoid re-proposing a broken fix. The prompt explicitly instructs: read the `[reason: ...]`, do not re-propose an action whose error shows it can't work, lower/raise confidence based on past outcomes.
- **AVAILABLE FIX ACTIONS** (`prompt.rs:38-54`): an enumerated catalogue with exact JSON shapes — must match `FixAction` variant tags.
- A long body of **policy/guardrail prose** (`prompt.rs:56-166`): investigate-before-acting evidence rules; `file_delete` two-condition gate; least-destructive/never-uninstall; a NORMAL-behaviour denylist (DCOM 10016, update chatter, VPN toasts, GPU telemetry); a hard rule that high CPU/RAM/disk *usage* is not a fault absent OOM/exhaustion; never-disrupt-the-user for `process_kill`/`service_stop`; a SECURITY POSTURE section mapping firewall/Defender state to the safe security actions, with third-party-AV/firewall and null-means-unknown caveats; a ≥0.80 confidence + ≤5-problems rule.
- **`needs_deeper_analysis` instruction** (`prompt.rs:162-166`): the model sets it true when evidence is ambiguous/conflicting and it can't confidently act at this reasoning level — the advisor escalation trigger.
- A strict **JSON-only output schema** (`prompt.rs:168-186`).

### Decision types & fix parsing (`models.rs`)

- `ClaudeDecision` (`models.rs:232-242`): `analysis: String`, `problems: Vec<Problem>`, `needs_deeper_analysis: bool` (`#[serde(default)]` so terse/old responses still parse).
- `Problem` (`models.rs:115-124`): diagnosis, root_cause, `confidence: f32`, `proposed_fix: serde_json::Value` (kept as raw JSON), reasoning, side_effects, undo_instructions. `parse_fix_action` (`models.rs:127-129`) deserializes `proposed_fix` into `FixAction`, returning `None` on mismatch — so a malformed/unknown action is dropped rather than executed.
- `FixAction` (`models.rs:133-207`): `#[serde(tag = "action", rename_all = "snake_case")]` enum across ~20 actions (service/log/disk/task/registry/network/driver/bcd/process/file plus Phase-5 security actions). `PowerShellDiagnostic` pins `#[serde(rename = "powershell_diagnostic")]` because snake_case would otherwise yield `power_shell_diagnostic` (`models.rs:153-156`). `SoftwareUninstall` still exists as a variant though the prompt explicitly forbids uninstalling.
- `CallUsage` (`models.rs:245-252`): input/output tokens, cache_creation, cache_read, `cost_usd`.

### Advisor escalation tiers (`main.rs` + `config.rs`)

`AdvisorConfig` (`config.rs:26-50`): `enabled` (default false), `escalation_model`, `escalation_effort`, `low_confidence_threshold` (default 0.6, clamped 0.0–0.95 on update). Effort is normalised to `low|medium|high|xhigh|max` or empty (`config.rs:156-161`). The tier is **fixed config, never AI-chosen**.

`should_escalate` (`main.rs:271-302`) is pure and returns `Some(reason)` only when **all** hold: advisor enabled; at least one lever set (model or effort); `escalations_today < MAX_ESCALATIONS_PER_DAY` (24, `main.rs:265`); **and** either `needs_deeper_analysis` is true ("ambiguous") or there is at least one problem whose max confidence is below `low_confidence_threshold` ("confidence was low"). A healthy/empty result never escalates.

Escalation flow (`main.rs:1122-1179`): runs the base `analyze` first, then at most one deeper `analyze_with(..., Some(escalation_model), Some(escalation_effort))`. The attempt is counted **before** the call (`advisor_escalations_today += 1`, `main.rs:1147`) so a failing escalation can't retry every cycle and defeat the cap. On success the deeper `ClaudeDecision` **replaces** the base one. The day-counters (`advisor_spent_today`, `advisor_escalations_today`, `advisor_spend_date`) reset on a UTC date change (`main.rs:1124-1129`).

### Usage / cost accounting

Per-provider usage extraction in `client.rs`: OpenRouter reads a streamed final usage chunk (prompt/completion tokens + `cost` USD where reported); Anthropic native parses token counts from the SSE events and **estimates** cost from list pricing; the Claude CLI reports its envelope usage including cache tokens and the equivalent API cost (no actual charge — subscription); the Kilo CLI reports its NDJSON `step_finish` tokens and cost the same way (also no actual charge — subscription; cost may be absent on some event shapes → 0). Both base and escalation calls log via `audit::log_usage` into the `usage_log` table and refresh `audit::usage_summary` (24h + 7d aggregate of calls/tokens/cost) into broadcast status. Escalation cost additionally accrues to `advisor_spent_today` for visibility, but the only remaining escalation backstop is the hard 24/day count cap.

### Web-search path (used by the updater, not monitoring)

`AiClient::complete` is a separate entry point for the app-updater to resolve installer URLs / read failures with live web search where the provider supports it. **OpenRouter** uses its `web` plugin (`call_openrouter_web`, non-streaming). **Anthropic** uses the native `web_search_20250305` server tool (`call_anthropic_web`, non-streaming, max 5 searches at ~$0.01 each folded into the cost estimate; bails clearly if the turn produced no text); the model is resolved by `anthropic_web_model` — blank/bare-alias/non-Claude ids fall back to `claude-haiku-4-5`. **Claude CLI** uses the CLI's built-in web search; `claude_cli_model` coerces blank/non-Claude ids to the `haiku` alias. **Kilo CLI** does its own web search as part of its `--auto` agent loop, same as the base analysis call — the updater's plan validator, download gates, and signature policy still bound anything it returns regardless.

`AiClient::complete_text` is the no-web sibling used by the learned-fact labeller (`learn/label.rs`) — a plain completion so a one-sentence label can't spend budget on searches.

## Executor, policy, safety & explanations

This subsystem turns an AI diagnosis into a *safe, auditable* side effect. It is the layer between Claude's `proposed_fix` and the actual machine mutation. Four cooperating modules:

- `eir-svc/src/executor/` — the only code that mutates the system (the infrastructure adapters).
- `eir-svc/src/policy/mod.rs` — the gate deciding auto / approval / blocked.
- `eir-svc/src/safety.rs` — rate-limiting and aggregate success-rate stats over the audit DB.
- `eir-svc/src/explain.rs` — deterministic, AI-independent descriptions of what an action does.

The orchestration that ties them together lives in the decision loop in `eir-svc/src/main.rs` (around lines 1205–1340) plus the executor worker `spawn_executor` (`main.rs:407`).

### Control & data flow

Per diagnosed problem (`main.rs:1208`):

1. `problem.parse_fix_action()` → `Option<FixAction>`. Unknown action ⇒ surfaced as a non-fixable problem and skipped (`main.rs:1215`).
2. `pol.evaluate(&action, problem.confidence)` → `Verdict` (`policy/mod.rs:49`).
3. Route on the verdict:
   - **`Block(reason)`** — recorded as a problem with the reason; nothing runs (`main.rs:1231`).
   - **`AutoApprove`** — guarded by `safety::rate_limited` (`main.rs:1246`) and an in-process `st.in_flight` dedupe set (`main.rs:1262`); then handed to the executor worker via `exec_tx.send(ExecJob{…})` and the loop moves on (`main.rs:1270`).
   - **`RequireApproval(reason)`** — builds an `ApprovalInfo` from `explain::explain` + `explain::target_details`, persists a pending-approval row, and pushes a `PendingApproval` card to UI state (`main.rs:1283–1336`). Non-blocking; dedup-guarded against both pending cards and in-flight actions.
4. The executor worker is a single serialised task draining an unbounded mpsc queue. It isolates and times out `executor::execute`, then atomically commits the execution/decision/approval transition (including exact registry undo when required), records feedback, and reports an `ExecOutcome` back to the loop. Panic/timeout results take the same audited path and requeue an approved row. **Design decision:** execution stays off the decision loop so UI/status remains responsive regardless of action duration.

`executor::execute` (`executor/mod.rs:15`) is a single big `match` over `FixAction`. Two execution styles:
- **`blocking(...)`** (`mod.rs:130`) wraps synchronous Win32/`std::process` work in `tokio::task::spawn_blocking`, mapping a join panic to an `Err`. Used by `services`, `logs`, `tasks`, `registry`.
- Direct `.await` of an async adapter (`driver`, `software`, `boot`, `process`, `security`, and the inline PowerShell variants) via `make_result(...)` (`mod.rs:140`).

`make_result` normalises `anyhow::Result<String>` into `ExecutionResult { action: format!("{action:?}"), success, output }`. The Debug form remains the human-readable audit label and legacy rate-limit fallback. Durable deduplication and new execution-rate matching use `FixAction::dedup_key`, which deliberately ignores regenerable parameters while retaining the action type and semantic target.

### FixAction implementations

22 variants (`models.rs`), each with a guard appropriate to its blast radius (the last —
`StartupSet` — was added in v0.24.0 for the user-initiated startup advisor and is never
AI-proposed):

| Action | Adapter | Mechanism | Built-in guard |
|---|---|---|---|
| `ServiceRestart/Stop/Start` | `services.rs` | Win32 SCM API; stop/start wait up to 30s for the requested state | critical-service blocklist; empty/control/NUL/slash service names rejected before UTF-16/SCM conversion; observed final state required |
| `LogCleanup{path,days_old}` | `logs.rs` | active-user-impersonated `walkdir` delete of bounded extensions older than cutoff | local absolute non-reparse path; `days_old ≥ 1`; protected-dir checks; no-effect/read-error/time-limit runs fail instead of rate-limiting a false success |
| `DiskCleanup{target}` | inline PS (`mod.rs:35`) | only `temp`/`tmp`/`prefetch` mapped; an unknown target returns a real **failure** (not a success-shaped no-op that would poison the rate limiter) | hardcoded target switch |
| `PowerShellDiagnostic{script}` | `powershell::run_diagnostic` | arbitrary script as SYSTEM | **none** — full machine access; kept off whitelist |
| `TaskDisable/Enable{task_name}` | `tasks.rs` | `Disable/Enable-ScheduledTask` + state readback | fully-qualified path required; glob refusal; policy denies all `\Microsoft\Windows\…` tasks |
| `RegistryReset{key,name,data}` | `registry.rs` | exact typed snapshot → write/readback; failure rolls back immediately; execution and durable undo commit together | machine-only Tcpip/Multimedia allowlist; only String/ExpandString/DWord/QWord; unsupported/read failures abort before writing; undo first verifies the live type/data still equals Eir's applied value and refuses to overwrite a newer change |
| `NetworkDiagnostic{command}` | inline PS (`mod.rs:68`) | only `flush_dns/release_renew/reset_tcp/reset_winsock`; else early-return failure | hardcoded command switch |
| `DriverDisable{name}` | `driver.rs` | `sc.exe config … start= disabled`, then registry start-mode readback | critical-driver blocklist + bounded service-name charset |
| `DriverEnable{name}` | `driver.rs` | `sc.exe config … start= demand`, then readback | bounded service-name charset (auto-whitelisted) |
| `SoftwareUninstall{pkg}` | `software.rs` | Get-Package → registry uninstall string / msiexec | **also hard-blocked in policy** — never runs |
| `BcdEdit{element,value}` | `boot.rs` | `bcdedit /set {current}` | **`SAFE_ELEMENTS` allowlist** + shell-metachar rejection on value |
| `ProcessKill{name}` | `process.rs` | exact process lookup → forced stop → bounded wait for exit | protected-process blocklist (including `.exe` spelling) + glob refusal |
| `FileDelete{path}` | `logs.rs` | active-user-impersonated native single-file deletion with existence postcheck | approval-gated; local absolute non-reparse path; directory/protected/UNC refusal; preview uses the same user token, so a mutable user path never becomes a privileged SYSTEM deletion |
| `FirewallEnable{profile}` | `security.rs` | `netsh advfirewall set … on`, then profile-state query | profile allowlist + observed postcondition |
| `DefenderSignatureUpdate` | `security.rs` | `Update-MpSignature`, then signature-age query | observed postcondition |
| `DefenderRealtimeEnable` | `security.rs` | `Set-MpPreference -DisableRealtimeMonitoring $false` | approval-gated (could conflict with 3rd-party AV) |
| `SfcScan` / `DismRestoreHealth` | `repair.rs` | `sfc /scannow` / `DISM …/RestoreHealth` (long timeout) | approval-gated, never whitelisted |
| `StartupSet{name,location,hive,enable}` | `startup.rs` | writes and reads back the `StartupApproved` REG_BINARY flag | closed locations; active interactive SID must match per-user entries; underlying Run value/shortcut must still exist; leaf/glob guards; approval-gated |

Adapter-level guards are **defence in depth**: they enforce regardless of policy.toml, mostly via const allow/block lists plus single-quote escaping (`'` → `''`) before string interpolation into PowerShell.

### PowerShell timeout helper

`executor/powershell.rs`. `run_diagnostic(script)` calls `run_diagnostic_with_timeout` with `DEFAULT_TIMEOUT = 120s` (`powershell.rs:7`). Spawns `powershell.exe -NonInteractive -NoProfile -ExecutionPolicy Bypass -Command <script>`, stdin nulled, `kill_on_drop(true)`. Wraps `child.wait_with_output()` in `tokio::time::timeout`; on timeout returns an error and the dropped future kills the process. Non-zero exit ⇒ error carrying stdout+stderr+code. The 120s ceiling bounds the executor worker (`security.rs:36` notes even the Defender pull uses the default cap deliberately).

All PowerShell-based adapters — including `registry.rs` and `tasks.rs` — route through `run_diagnostic`, so every one is bounded by the 120s timeout with `kill_on_drop`; none spawns an untimed `std::process::Command`.

### Policy: the Verdict gate

`policy/mod.rs`. `ExecutionPolicy::load` parses `policy.toml`. `Verdict` (`mod.rs:36`): `AutoApprove | RequireApproval(String) | Block(String)`. `evaluate(action, confidence)` applies, in strict order (`mod.rs:49`):

1. **Action-type blocklist** (`blocklist.actions`) ⇒ `Block`. Never runs, *not even with approval*. Currently only `software_uninstall`.
2. **Target blocklist** (`blocked_reason`, `mod.rs:84`) ⇒ `Block`. Service-name blocklist (case-insensitive exact match) for service actions; path blocklist (case-insensitive **prefix** match) for `LogCleanup`, `RegistryReset`, `FileDelete`.
3. **Confidence gate** — `confidence < confidence_threshold` ⇒ `Block` (does not prompt).
4. **Whitelist** — action type not in `whitelist.actions` ⇒ `RequireApproval`.
5. Otherwise ⇒ `AutoApprove`.

Ordering is the key invariant: blocklist beats whitelist (a tested property — `blocklisted_action_is_always_blocked`, `mod.rs:154`), and confidence is checked before the whitelist so a low-confidence whitelisted action is blocked rather than auto-run. `confidence_threshold` is the only live `ExecutionConfig` field; it is overwritten at startup from `config.toml`/Settings (`main.rs:613`, `pol.execution.confidence_threshold = cfg.monitoring.confidence_threshold`). `policy.toml`'s value is a fallback.

**Auto vs approval vs blocked (current policy.toml):**
- **Auto** (whitelisted, reversible/low-risk): `service_restart/stop/start`, `log_cleanup`, `disk_cleanup`, `task_disable/enable`, `network_diagnostic`, `driver_enable`, `firewall_enable`, `defender_signature_update`.
- **Approval** (off whitelist on purpose): `registry_reset`, `powershell_diagnostic`, `driver_disable`, `bcd_edit`, `process_kill`, `file_delete`, `defender_realtime_enable`, and user-initiated startup changes.
- **Blocked outright**: `software_uninstall` (one-way door — no reinstall path). Plus any target hitting the service/path blocklists (e.g. NTDS, WinDefend, `C:\Windows\System32`).

### Safety: rate-limiting & success rate

`safety.rs`. `rate_limited(pool, action, window_mins)` sums successes and counts rows in `execution_log` where `action = format!("{action:?}")` AND `executed_at > cutoff`, and suppresses the action when **either**:

- it already **succeeded** in the window (no point re-running an applied fix), or
- it **failed ≥ 3 times** in the window (`FAILURE_BREAKER_THRESHOLD` — the circuit breaker added in v0.17: a persistently failing auto-action backs off until the window rolls over instead of retrying every cycle).

Both behaviours are covered by unit tests against the real migrations. The match key is the exact Debug string, so two actions differing only in a field value are distinct keys.

`success_rate(pool)` (`safety.rs:24`) = `SUM(success)/COUNT(*)` across all of `execution_log`, returning `1.0` when empty. Used only for logging/warning in the loop (`main.rs:1195`, warns under 85%) — it does **not** feed back into any verdict.

### Explanations

`explain.rs`. `ActionExplanation { summary, target, reversible }` (`explain.rs:13`). `explain(action)` (`explain.rs:23`) returns a hand-written, deterministic description per variant — derived only from the action type and its fields, **never from the AI** — so the user can trust it when approving. The AI's own `side_effects`/`undo_instructions` are shown alongside as supporting detail (`main.rs:1308`). Notable: `software_uninstall`'s summary states it is policy-blocked and won't run; `powershell_diagnostic` uses `with_target_first_line` (`explain.rs:171`) to put a 60-char script snippet in `target`.

`target_details(action)` (`explain.rs:227`) gathers factual on-disk detail — for `FileDelete` it runs `file_facts` (size, last-modified+age, read-only flag, and a `classify_file` risk heuristic distinguishing regenerable cache vs personal-folder data vs config); for `PowerShellDiagnostic` it returns the full script. Does file I/O, so it is called only on the approval path, off the hot loop.

### Current gaps / dead code

- `max_retries_per_issue` and `auto_approve_on_success_rate` (`ExecutionConfig`, `mod.rs:17/19`) are **dead** — deserialized and referenced only inside `policy/mod.rs` tests; no production code reads them (confirmed by crate-wide grep). The `#[allow(dead_code)]` comment claims "used in Phase 4" but they are not. There is consequently **no success-rate-driven auto-approval promotion** (the retry problem itself is now bounded by the failure breaker in `safety.rs`).
- The executor worker adds a 10-minute backstop around every normal job (with the longer repair timeout for SFC/DISM) in addition to adapter-specific timeouts. Panic/timeout results are durably audited and approved jobs are returned to the approval queue.
- `RegistryReset` has the one specialised durable undo path described above. There is no generic undo mechanism for the other actions; `BcdEdit` remains non-reversible because its prior value is not snapshotted.

## Autonomous app updater

The updater is an AI-driven, self-healing, fully-unattended app updater that lives entirely in the LocalSystem `eir-svc` (so package managers and installers run with no UAC prompt). It is internally layered, dependencies pointing inward (per `eir-svc/src/updater/mod.rs:1`):

- a pure **domain / validation core** (`domain.rs`, `plan.rs`, `version.rs`, `names.rs`) with no I/O — the "AI proposes, Rust disposes" layer every AI proposal must pass;
- an **application orchestrator** (`orchestrator.rs`, `check.rs`, `diagnose.rs`) — the check → attempt → diagnose → retry loop;
- per-method **infrastructure adapters** (`methods/winget.rs`, `choco.rs`, `scoop.rs`, `msstore.rs`, `native.rs`, plus `download.rs`, `verify.rs`, `proc.rs`, `history.rs`).

### The cycle

One full cycle is `run_cycle` (`orchestrator.rs:303`), driven from `main.rs:307` (`spawn_update_cycle`) on a detached task with a 60-minute backstop watchdog and a `cycle_id` = `Utc::now().timestamp()` that groups the run's rows in the audit DB. The cycle:

1. **Determine available methods** — `available_methods` (`orchestrator.rs:207`): a method is usable only if enabled in config AND safe in the SYSTEM service. winget resolves through PATH, `C:\Program Files\WindowsApps`, then `Get-AppxPackage -AllUsers`; choco uses its ProgramData path and can be bootstrapped through the official install script; msstore reuses winget; `Native` requires `native_enabled` and an AI client. Scoop is fail-closed: its user-owned `.cmd` shim is never discovered or executed by EirSvc, and `detect::scoop_available` always returns false until a real active-user process launcher exists.
2. **Collect candidates** — `check::collect` (`check.rs:97`). Each available manager lists its updates (`winget upgrade`, `choco outdated -r`, `winget upgrade --source msstore`). Results are de-duplicated by app identity via `push_candidate` (`check.rs:66`): earlier (more-preferred) managers win, the id is `clean_app_name(name).to_lowercase()`, and `should_skip` / the seen-set drop ignored/duplicate apps. When a primary manager handles the app, the native installer is appended as a self-healing fallback method (unless the primary already is native). Then an **AI web-search pass** over apps no manager covers (`check_unmanaged`, `check.rs:244`) produces native-only candidates. Both this discovery pass and the native-installer lookup go first to GitHub's canonical `/owner/repo/releases/latest` redirect for GitHub-hosted software, avoiding guessed tags or stale generic release pages. The unmanaged inventory is now independent of winget: `inventory::list_installed` (`inventory.rs`) enumerates the HKLM + Wow6432Node + per-user Uninstall registry keys, and the result is merged with `winget list` when winget is present. If winget is missing or the AI/native path is disabled, a UI note explains the degraded coverage instead of staying silent.
3. **Heal each candidate** (bounded by `max_apps_per_run` and `max_attempts_per_app`) — `heal` (`orchestrator.rs:134`).
4. **Record** every attempt to the `update_attempts` table under `cycle_id` (`history::record_attempts`).

`CheckResult`/`CycleSummary` carry candidates, per-cycle AI cost, coverage state, and human notes for truncation, source/check failures, or deferred work. `app_rows` flattens each candidate's attempts into one UI row, the winning attempt (first success, else last) deciding state: `current` / `verified` / `installed` / `failed` / `skipped`.

### Per-app multi-method self-heal

`heal` builds the candidate's method order (intersection of `candidate.methods` and `available`, preserving preference order), then loops: dispatch a method, classify the outcome, and on a **non-terminal** failure ask `decide_next` for the next step, repeating until success, a terminal integrity failure, methods exhausted, or `max_attempts_per_app` reached.

- `dispatch` (`orchestrator.rs:52`) applies any allow-listed remedy (`KillProcess` → sanitised `taskkill /IM <name> /F`, or `Force`) then calls the method adapter.
- `decide_next` (`orchestrator.rs:111`): when an AI client is configured it calls `diagnose::diagnose` (the diagnostician), otherwise the deterministic ladder `next_method`.
- **The AI is bounded by attempt caps**: `max_attempts_per_app` limits how many methods are tried per app, and `max_apps_per_run` limits how many apps are acted on per cycle. `decide_next` is called to choose the next method after a non-terminal failure (`orchestrator.rs:180`).
- **Reboot is never taken unattended**: a `RetryAfterReboot` remedy ends the heal (defer) rather than rebooting (`orchestrator.rs:195`).

**Rust always has the final say.** The AI diagnostician (`diagnose.rs`) is shown the *real* captured error, the failure category (classified by Rust, not the AI), and the tried/available methods, and proposes ONE `ProposedStep`. `validate_next_step` (`domain.rs:238`) disposes: an integrity-terminal failure always gives up; a `Switch` target must be available and untried; a `Retry` remedy must fit the method and failure (`remedy_ok`, `domain.rs:269`) — e.g. `Force` only on winget/choco, `ClearManagerLock` only on choco/scoop, `KillProcess` only when the name appears as a whole token of the error text; anything invalid falls back to `deterministic_next`. A malformed AI reply collapses to GiveUp, which the validator then turns into the deterministic step, so a bad reply never strands an app.

### Method order and adapters

The persisted/default method token order remains `winget, choco, scoop, msstore` for config and wire compatibility, but the UI disables Scoop and `available_methods` removes it unconditionally. `native` is gated separately by `native_enabled` and appended as a per-candidate fallback. Each adapter returns a structured `AttemptOutcome`:

- **winget** (`winget.rs`): resolves `winget.exe` through `detect::winget_path` (PATH alias, WindowsApps MSIX, or `Get-AppxPackage -AllUsers`), runs directly as SYSTEM, captures and cleans winget's output (`clean_winget_output`, ported verbatim with tests for spinner/OEM-mojibake/byte-counter stripping), auto-retries once with `--force` for the portable-modified case, verifies by id.
- **choco** (`choco.rs`): `choco outdated -r` (pipe-delimited, pinned packages skipped), `choco upgrade <id> -y`, success codes 0/3010/1641, cross-checks the new version via winget's ARP read.
- **scoop** (`scoop.rs`): disabled in the SYSTEM service. Listing returns an explicit unavailable error and update attempts return `Blocked`; no `C:\Users\*\scoop\shims\scoop.cmd` path is selected or launched.
- **msstore** (`msstore.rs`): `winget ... --source msstore`; per-user, may need the user's Store entitlement.
- **native** (`native.rs`): the AI-found installer path (below).

### proc timeouts

All external commands go through `proc::run_capped[_cmd]` (`proc.rs`), which applies `CREATE_NO_WINDOW` and a hard timeout with `kill_on_drop` so a hung child (a known winget-as-SYSTEM failure mode) can never wedge the cycle and latch `updater_running` forever. On overrun it returns the sentinel `TIMED_OUT = -4` with "timed out" text (classified as transient/non-terminal). Timeout constants: `PROBE = 30s` (presence probes), `LIST = 150s` (update listing / source refresh), `INSTALL = 600s` (download + run installer), `VERIFY = 60s` (read installed version back, signature read, exe ProductVersion read).

### Native AI-found installer + signature policy

For an app no manager can update, `update_native` (`native.rs:122`): asks the model for the OFFICIAL direct installer (`install_plan_prompt`), validates the plan, downloads + hashes + signature-gates it, runs it as SYSTEM, and verifies the version moved. Nothing the AI returns reaches the shell unchecked:

- **`validate_plan`** (`plan.rs:222`, pure, unit-tested): https-only; no credentials/non-default-port/raw-IP/punycode host; host must be a TRUSTED multi-tenant release host (`github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com` — deliberately NOT `*.github.io` or `raw/gist.githubusercontent.com`, which serve arbitrary user files) OR the app's own vendor domain by **exact brand-label equality** (`host_matches_name`, rejecting lookalikes like `obsidian-download.com`, `notionx.io`, `krita.evil.com`); URL must end `.exe`/`.msi`/`.zip`; an optional ZIP member must be a safe relative `.exe`/`.msi` path; silent args allow-listed by installer kind via `sanitise_args` (shell metacharacters dropped); optional 64-hex SHA-256. An `.exe` with no known silent switch is refused (`plan_runnable`) → manual fallback; an MSI defaults to `/qn /norestart`.
- **`download_and_check`** (`download.rs:345`): streams to a SYSTEM/Administrators-only staging dir under `%ProgramData%\Eir\staging` (ACL applied **fail-closed** — no install if lockdown fails; `ensure_root`). The full URL gate is re-applied on every redirect hop and the final URL; HTML bodies and over-cap sizes are rejected (header and streamed-byte counter); a vendor SHA-256, if given, must match the downloaded asset (terminal `HashMismatch`). For ZIP assets, only the model-named member—or the sole unambiguous `.exe`/`.msi`—is extracted to a fixed staging filename; invalid paths, more than 10,000 entries, ambiguous installers, and decompressed installers over the configured size cap are refused. The extracted installer then passes the same Authenticode gate and pre-launch rehash as a direct asset.
- **Signature gate** (`signature_gate`, `download.rs:167`) is a HARD gate decided in Rust before launch, per `SignaturePolicy` (`config.rs:14`): `RequireValid` (default — any trusted valid Authenticode signature), `RequirePublisherMatch` (valid AND signer CN equals the expected publisher — note: `expected_publisher` is currently AI-sourced, so this is a tripwire, not a true vendor pin), `AllowUnsigned` (explicit opt-in). A rejection message starts "signature rejected" so it classifies as terminal `SignatureRejected`. A timed-out signature read yields non-"Valid" text → fails closed.
- **`run_installer`** (`native.rs:243`): re-hashes the staged file immediately before launch (TOCTOU; sentinel `-4` = changed → reported as tampering/`HashMismatch`), runs the exe directly or via `msiexec /i`, with a 10-minute watchdog. Success = exit 0 or 3010 (reboot). Verification is by name (winget ARP read) with an exe-ProductVersion fallback, softened to `Unverified` on an exe-fallback mismatch since a 4-part FILEVERSION can trail the marketing version.

### Self-updater skip (should_skip / base_id / SELF_UPDATING) and the ignore list

Two distinct skip mechanisms:

1. **`SELF_UPDATING` / `should_skip` / `base_id`** (`check.rs:35`–`60`) — the just-added skip for apps that update themselves and reliably fight package managers. `SELF_UPDATING = &["discord"]` (Discord's Squirrel installer hangs `choco upgrade` for the full INSTALL timeout, then choco's stale version DB makes it retry every cycle). `base_id` strips a Chocolatey package suffix (`.install`/`.portable`/`.app`/`.commandline`) so `discord.install` and `discord` share one identity. **`should_skip` unifies the self-updater set and the user ignore list against the same base id**: an id is skipped if its base is in `SELF_UPDATING`, OR if the user's `ignored` list matches the exact id or its base (case-insensitive) — so ignoring `discord` also covers the `discord.install` choco package, and vice versa. `should_skip` is enforced at three points: when pushing manager candidates (`push_candidate`, `check.rs:78`), when filtering the unmanaged set before the AI check (`check.rs:232`), and on the AI's returned native candidates (`native_candidates_from`, `check.rs:310`).
2. **Eir-self skip** — the `is_noise` SKIP list in `winget_parse.rs:130` excludes `"eir"` (alongside drivers/runtimes/redistributables/OS components) from the unmanaged AI check, so the app updater never tries to update Eir itself; Eir's own self-update is handled by the separate Tauri updater plugin, not this engine.

Native candidate identity is anchored to the machine (`native_candidates_from`, `check.rs`): an AI-reported update whose name doesn't resolve unambiguously to an actually-installed app (`match_installed_entry` against the merged winget + registry inventory set) is dropped, and only strictly-newer versions (`version::is_newer`, with the marketing-truncation guard) survive — preventing a poisoned AI from naming a fabricated app to pick an arbitrary vendor domain.

Per-app AI guidance reuses the updater's existing `[updater.notes]` config map. The App Updates view can create it from a detected row and read/edit/delete it later from the persistent saved-guidance list. The stable, version-stripped installed identity—not an AI-returned alias—is the key. Guidance is included in both the unmanaged version-check prompt and native-installer prompt, but cannot relax Rust's URL, hash, Authenticode, size, or silent-install gates. Service-side limits cap ids at 200 characters, notes at 2,000 characters, and the map at 500 entries.

### update_attempts history

`history::record_attempts` inserts one row per attempt into `update_attempts`: app identity, from/to versions, method, outcome/category, exit code, signature/hash evidence, detail, cost, and timestamp. `recent` exposes the useful result evidence to the UI. Separate durable state records current-cycle evidence, the last completed run, and the last fully clean run. Clear writes a display cutoff: it hides older attempt rows but preserves scheduler fairness, last-run/last-clean timestamps, and cycle evidence. `AlreadyCurrent` is a successful `current` state, distinct from an update Eir installed.

### update_checks rotation (unmanaged AI sweep)

The unmanaged AI check is capped at `AI_CHECK_CAP` (20) apps per cycle. The inventory merges bounded HKLM, 32-bit, and real per-user uninstall hives with `winget list`; service-account hives and malformed/oversized rows are excluded, while partial-source failures remain visible as warnings. `update_checks` (migration 0016) stores the last successfully parsed AI check per app. `check_unmanaged` sorts stalest/never-checked first and records a batch only after a valid response, so the tail rotates without presenting a failed parse as completed coverage. Any recorded source failure, deferred candidate, or incomplete native coverage prevents the run from being labelled clean. Availability probing can omit an enabled but missing manager when another source is usable; see the current limitations.

## User-facing on-demand tools (Ask / Timeline / Disk / Startup)

Added in v0.24.0 (features F10–F13), these turn the existing signal/AI/executor stack into
things the user *opens on purpose*. All four follow the same seams: new `#[serde(default)]`
`StatusPayload` fields + `UiMsg` variants in `eir-proto`; a correlated Tauri command in
`eir-ui` (with neutral protocol-v1 fallback); a `ui/main.js` renderer (all service strings
through `esc()`/`escAttr()`);
and — where a scan is involved — an **off-loop task** mirroring the analysis/digest hardening
(inner `tokio::spawn` under `tokio::time::timeout`, a `*_running` flag released on success,
panic, *and* timeout, result over a dedicated mpsc arm).

**The trust invariant they all share:** only a client in the active interactive session
may use the command pipe, and a UI message never carries a `FixAction` or a raw path—it
carries an **opaque id**. The service maps that id to an action *from its own last-scan state*
(`st.disk_targets` / `st.startup_targets`) and routes it through the same `pol.evaluate`
gate as an AI-proposed fix (`route_user_action`, `main.rs`). An unknown/stale id is ignored.

### F11 — Health timeline (`audit::metric_history` + `ui` sparklines)

`audit::metric_history(pool, cutoff_rfc3339, cap)` reads the previously UI-invisible
`system_state_history` series (the same one the trend detector uses), oldest-first, and
`thin_points` downsamples it to ≤ `cap` (200) points keeping the endpoints — both pure and
unit-tested. `st.history` is refreshed once per decision tick (not per broadcast, so
`build_status` stays a cheap clone). The dashboard renders three hand-built inline-SVG
sparklines (CPU/mem/disk, 0–100%) with marker dots at problem/execution timestamps — no chart
library, consistent with the no-dependency frontend.

### F10 — "Ask Eir" (`ask.rs`, off-loop `complete_text`)

A free-text question is answered with live context. `ask::ask_rejection_reason` (pure) gates
each request (empty / >1000 chars / no provider / already running / <15 s since the last — a
spend guard on the open pipe). On accept, the loop snapshots the context (metrics, failed
services, recent problems/executions) and spawns an off-loop task that gathers the DB-derived
trend + learned facts, builds a bounded prompt (`ask::build_prompt`, pure — instructs the model
to answer diagnostically and **never** emit commands/actions), and calls `AiClient::complete_text`
(main/default model, no web search — the labeller/digest entry point). The answer is
display-only; **nothing is parsed or executed from it** — fixes still come only from the decision
cycle. History (`st.ask_entries`) is memory-only (cap 10, newest first), lost on restart. As of
v0.29.0 the last 5 Q&A pairs are folded into the prompt as "Previous conversation" context, so
follow-ups keep continuity, and a `ClearAsk` message resets the chat.

### F12 — Disk-space insights (`disk_scan.rs`)

An on-demand scan (`ScanDisk`) ranks the biggest space consumers on the system drive — "what's
eating my SSD" (SSD era: space is the scarce resource, not fragmentation). `scan(deadline)` runs
in `spawn_blocking` (walkdir, `dir_size` bounded by an internal deadline + a join-timeout backstop;
reparse points are skipped to avoid cycles/double-counting). `system_specs()` is a **pure,
unit-tested** list of targets with deterministic, hand-written categories and notes (the
`explain.rs` philosophy — never AI-authored). **Cleanup maps only to pre-existing safe actions:**
`DiskCleanup{temp|prefetch}` (whitelisted → auto-runs) and a stray `MEMORY.DMP` → `FileDelete`
(approval-gated, with the existing `file_facts` risk classification). Everything else
(`Windows.old`, browser/app caches, Recycle Bin, hiberfil/pagefile, per-user temp, largest
`AppData\Local` folders) is **report-only** in v1. A "Clean" click sends the entry id;
`CleanDiskEntry` maps it via `st.disk_targets` and routes through `route_user_action`. No AI call.

### F13 — Startup advisor (`startup_scan.rs` + `executor/startup.rs` + `executor/tasks.rs`)

`ScanStartup` (rebuilt **Autoruns-style in v0.25.0**) enumerates the auto-start extension
points: Run keys (per-user + machine + the **HKLM Wow6432Node** 32-bit view), Startup-folder
`.lnk`s (targets resolved via `WScript.Shell` — confirmed session-0-safe, it's `wshom.ocx`,
not the Explorer-bound `Shell.Application`), one-shot **RunOnce** keys, **`Policies\Explorer\
Run`** keys, **Winlogon Shell/Userinit anomalies** (emitted only when non-default — a clean
machine adds no noise), **logon-triggered scheduled tasks** outside `\Microsoft\`, and
**auto-start services** whose binary lives outside `<drive>:\Windows\` (regex anchored at the
drive root, optionally quoted, so `C:\Users\Public\Windows\evil.exe` can't masquerade its way
out of the listing). Because LocalSystem's `HKCU` is the SYSTEM hive, per-user data is read
only from the SID attached to the active WTS session under `HKEY_USERS`, never from every
loaded user's hive.

Each entry carries a **deterministic identity**: the launched binary is resolved from its
command line (quoted path / `.exe`-prefix / first token; **UNC targets are skipped** — probing
them from LocalSystem risks both a ~20 s/path SMB stall inside the 120 s budget and machine-
account auth to an attacker share) and its **Authenticode signer CN** (fallback: VersionInfo
`CompanyName`) is read once per unique path. `microsoft` (signer/company prefix, pure fn) feeds
the UI's default-on "Hide Windows" filter; `report_only` marks locations with no safe switch
(RunOnce, policy keys, Winlogon, services). The enumeration is one PowerShell pass emitting
compact JSON; `approvedByte` is coerced `[int]` in the script AND tolerated as string on the
Rust side (`int_or_string`) — PowerShell argument binding was observed live emitting `"-1"`,
which under a strict `i64` failed the whole array parse into a silent empty scan. PS
note-properties are stripped by exact name (a `-like 'PS*'` prefix filter dropped/evaded
legitimately-named `PS…` Run values). `decode_enabled` (pure) turns the StartupApproved first
byte into "enabled"; tasks/services carry an explicit `state` instead (`entry_enabled`). One
bounded `complete_text` call optionally explains each entry (keep / optional / unnecessary + a
≤35-word plain-English note: what it is and **where it likely came from**, grounded on the
signer and the location's abuse profile) — **advisory only, triggers nothing**; parse failures
leave the deterministic listing intact. Cap 100 entries; the script emits toggleable launch
points first so truncation sheds the report-only bulk (services) last.

Enable/disable: Run-key/folder entries use **`FixAction::StartupSet { name, location, hive,
enable }`** — a 12-byte `StartupApproved` REG_BINARY (`0x02` enabled / `0x03` disabled), the
same mechanism as Task Manager's Startup tab, **fully reversible**; `machine_run32` maps to the
`StartupApproved\Run32` subkey. Scheduled-task entries reuse **`TaskEnable`/`TaskDisable`**
with the full `\Folder\Name` path — `build_task_script` splits it into `-TaskPath`/`-TaskName`
so a same-named task in another folder can't be hit, and `run_task_cmd` **refuses wildcard
metacharacters** (`Get-ScheduledTask` globs `*?[]`; single-quoting stops injection, not
globbing — this guard also covers the AI path's bare names). `executor::startup`: `location`
is a **closed set** mapped to hard-coded keys (`approved_key`, `Registry::`-qualified); SIDs
are validated and re-resolved immediately before a per-user write; value names reject glob
metacharacters. The underlying Run value or startup shortcut must still exist before the
`StartupApproved` flag is written, and the resulting byte is read back.
`StartupSet` is **not in the AI prompt catalogue** and **not whitelisted**, so it lands on
`RequireApproval`; task toggles are whitelisted for the AI path, so `route_user_action` takes
`force_approval: true` from `SetStartupEntry` to downgrade their AutoApprove to
`RequireApproval` — every user-initiated toggle gets a human confirmation (the pipe is
user-writable). A `SetStartupEntry` click maps the id via `st.startup_targets`.

## Persistence, audit DB & the existing feedback loop

Eir's persistence layer is a single SQLite database, opened once at service start and shared as an `sqlx::SqlitePool` passed by reference into every read/write helper. There is no ORM and no repository abstraction: each table has hand-written `INSERT`/`SELECT` queries with positional binds, grouped by concern into four modules — `eir-svc/src/audit.rs` (decisions, executions, usage, approvals), `eir-svc/src/feedback/mod.rs` (before/after outcome scoring), `eir-svc/src/updater/history.rs` (autonomous-updater attempt log), and `eir-svc/src/learn/store.rs` (learned facts and user overrides). The schema lives in `migrations/*.sql` and is applied at boot via the embedded `sqlx::migrate!("../migrations")`.

This is the durable substrate Eir learns from. The original feedback loop still injects a human-readable feedback summary plus the last 5 decisions into the AI prompt each cycle, and the newer self-improvement layer derives conservative `learned_facts` from the same audit/update history. There is still no model fine-tuning or policy auto-adjustment; learning is represented as explicit, user-visible rows with conservative effects.

### Database bootstrap

`audit::init_db(path)` builds `SqliteConnectOptions` with `create_if_missing(true)`, connects a pool, and runs the embedded migrations. The DB path comes from `config.persistence.audit_db`. Migrations are currently versioned `0001`–`0019` and apply in order on every start.

### Full schema (every table)

**`decisions`** (`migrations/0001_initial.sql:1`) — one row per AI analysis that produced a result.
- `id INTEGER PK AUTOINCREMENT`
- `timestamp TEXT` (RFC3339 UTC)
- `signal_snapshot TEXT` — full `SignalSnapshot` serialized as JSON (event log, file changes, system state, decision history)
- `claude_response TEXT` — full `ClaudeDecision` JSON (analysis, problems[], needs_deeper_analysis)
- `confidence REAL` — max problem confidence across the decision
- `executed INTEGER DEFAULT 0` — flipped to 1 once any action from this decision runs
- `execution_output TEXT` — **declared but never written or read** (executions go to `execution_log` instead; dead column)

Written by `audit::log_decision`; `executed` is flipped inside the same `persist_execution` transaction that writes the execution record; read by `audit::get_recent_decisions`.

**`system_state_history`** (`0001_initial.sql:11`) — a metrics time-series row written alongside every decision.
- `id`, `timestamp TEXT`, `cpu_usage REAL`, `memory_usage REAL`, `disk_usage REAL`, `failed_services_count INTEGER`, `snapshot TEXT` (full `SystemState` JSON).
- Written inside `audit::log_decision`; CPU/memory/disk columns feed the dashboard timeline and resource-trend detector. The full JSON snapshot is retained but not mined.

**`execution_log`** (`migrations/0002_execution_log.sql`) — one row per fix-action execution.
- `id`, `decision_id INTEGER NOT NULL → decisions(id)`, `action TEXT` (the executed action string), `success INTEGER`, `output TEXT` (stdout/stderr or error text), `executed_at TEXT`.
- Written by `audit::persist_execution` in the executor worker. Read by `safety::success_rate` (aggregate), semantic rate limiting, and `feedback::recent_summary` for failure-reason text.

**`execution_feedback`** (`migrations/0003_feedback.sql`) — the before/after outcome record; the heart of the feedback loop.
- `id`, `execution_log_id INTEGER → execution_log(id)`, `action TEXT`, `succeeded INTEGER`
- `cpu_before REAL`, `memory_before REAL`, `disk_before REAL`, `failed_services_before INTEGER`, and `target TEXT` — captured at execution time
- matching after metrics plus nullable `resolved` — filled once the row is at least 120 seconds old; `resolved` records whether a targeted service fault cleared
- `improvement_score REAL` — computed when after-state is filled
- `recorded_at TEXT`
- Written by `feedback::record` after the execution transaction commits; after-states, score, and targeted outcome are filled by `feedback::update_after_states`; read by `feedback::recent_summary` and the conservative fix-effectiveness detector.

**`usage_log`** (`migrations/0005_usage.sql`) — per-call AI token/cost accounting (populated for every provider as of v0.17: OpenRouter and the Kilo CLI report cost, Anthropic's is estimated, both subscription CLIs' cost is an equivalent-cost figure with no actual charge).
- `id`, `timestamp TEXT`, `input_tokens`, `output_tokens`, `cache_creation`, `cache_read` (all INTEGER), `cost_usd REAL`.
- Written by `audit::log_usage` (`audit.rs:123`); aggregated by `audit::usage_summary` (`audit.rs:142`) over 24h / 7d windows into `UsageSummary { calls, tokens, cost }` shown in the UI.

**`pending_approvals`** (`0006`, extended by `0017_durable_approvals.sql`) — actions awaiting or durably claimed by the user. The **row id is the approval id surfaced to the UI**.
- `id`, `created_at TEXT`, `decision_id INTEGER → decisions(id)`
- `action_json TEXT` — serialized `FixAction`, executed verbatim on approval
- `info_json TEXT` — serialized `ApprovalInfo` for the UI (its `id` field overwritten from the row id on load)
- `baseline_json TEXT` — `SystemState` at proposal time, the "before" baseline for feedback once executed
- `action_key`, `status` (`pending|approved|rejected`), and `claimed_at` make a click an atomic database transition. A unique active-action index prevents duplicate cards. On load, legacy NULL keys are derived from `action_json` after semantic duplicates are removed. Approved rows reload and retry after a crash until the execution transaction removes them; synthetic failures atomically return them to pending. Rejected tombstones are never re-presented.

**`registry_undo`** (`0002`, extended by `0018_registry_undo_type.sql` and `0019_registry_undo_applied_value.sql`) stores both the exact prior scalar kind/data and the exact kind/data Eir applied. Legacy or incomplete rows fail closed. Undo runs its applied-value comparison and restore in one PowerShell invocation, refusing if the live value no longer matches Eir's write.

**`update_attempts`** (`migrations/0007_update_history.sql`) — append-only log of every autonomous-updater attempt, grouped by `cycle_id`.
- `id`, `cycle_id INTEGER` (groups one run), `app_id TEXT` (version-stripped identity), `name TEXT`, `from_version TEXT`, `to_version TEXT`, `method TEXT` (winget|choco|scoop|msstore|native), `success INTEGER`, `category TEXT` (failure `ErrorCategory` token, NULL on success), `exit_code INTEGER`, `signature TEXT` (Authenticode result, native), `sha256 TEXT` (installer hash, native), `detail TEXT` (cleaned reason), `cost_usd REAL` (AI spend attributable to the attempt), `created_at TEXT`.
- Written by `updater::history::record_attempts`; `recent` surfaces name, method, success, detail, versions, failure category, exit code, and time. Clear records `updater_history_cleared_at` and filters older display rows rather than deleting attempts, preserving fair-rotation inputs and durable run state. Signature, SHA-256, and cost remain persisted evidence but are not copied into the compact recent-attempt row.

**Indexes** (`migrations/0004_indexes.sql`, `0005`, `0007`): `execution_log(action)`, `execution_log(executed_at)`, `execution_feedback(cpu_after)`, `usage_log(timestamp)`, `update_attempts(app_id, created_at)`, `update_attempts(cycle_id)`.

### How the feedback loop works today (control + data flow)

1. **Decision logged.** Each cycle, after the AI returns, `audit::log_decision` writes the `decisions` row (returning `decision_id`) plus a `system_state_history` row (`main.rs:1181`).
2. **Approval or execution routed per problem.** For each problem, the proposed fix is policy-gated (`main.rs:1205`+). `RequireApproval` → `insert_pending_approval` persists it (`main.rs:1317`); auto-approve → handed to the executor worker.
3. **Execution + baseline capture.** The executor worker runs the action panic-isolated and timeout-bounded. `persist_execution` atomically writes `execution_log`, marks the decision executed, stores any required registry undo, and transitions its approved row. Only after that commit does `feedback::record` write the "before" row using the `SystemState` captured when the action was proposed.
4. **Settled after-state measurement.** Once a row is at least 120 seconds old, `feedback::update_after_states` fills its CPU/memory/disk/failed-service after metrics. Service actions also record, case-insensitively, whether the specific target left the failed-services set; non-service actions keep `resolved = NULL`.
5. **Scoring.** `improvement_score` = `cpu_delta*0.3 + mem_delta*0.3 + disk_delta*0.3 + failed_services_delta*10.0`, where each delta is `before - after`. The targeted `resolved` result, not this blended score, drives service-fix effectiveness learning.
6. **Feedback into the AI and learning.** `feedback::recent_summary(db, 10)` prefers “cleared/did not clear the targeted fault,” then falls back to the resource score; failures include a whitespace-normalised, bounded reason from `execution_log.output`. The summary enters the next diagnosis prompt. Separately, only measured service-target outcomes feed `FixIneffective`; non-service outcomes do not yet produce structured learned facts.
7. **Decision history into the AI.** Separately, `audit::get_recent_decisions(db, 5)` (`main.rs:1022`) reconstructs the last 5 decisions as `PastDecision { timestamp, diagnosis, confidence, fix_proposed }` (one entry per problem) and is embedded in the `SignalSnapshot.decision_history` *and* passed as `history` to `analyze`.
8. **Success-rate telemetry.** `safety::success_rate` (`safety.rs:24`) = `SUM(success)/COUNT(*)` over all of `execution_log`; logged each cycle (`main.rs:1195`) and warns below 85%. It is **observability only** today — it does not gate or auto-adjust anything. (`policy.toml`'s `auto_approve_on_success_rate = 0.95` and `max_retries_per_issue` are parsed but marked dead-code "Phase 4", `policy/mod.rs:14`.)

The closed loop is: execute → record before/target → wait for settlement → verify target and resource effects → summarise into the prompt and conservative detectors. Learning can only make Eir less aggressive.

### Config load/save and `to_ui_settings`

`config.rs` defines `Config { api, monitoring, persistence, updater (#[serde(default)]), advisor (#[serde(default)]) }`. The two `#[serde(default)]` sections let an older `config.toml` (written before those features) still parse — covered by round-trip tests (`config.rs:267`–331).

- **`load(path)`** resolves relative to the executable directory, parses TOML, sanitises legacy unsafe log roots, and recovers from the last parseable `.bak` if the live file is corrupt.
- **`save(config, path)`** serialises the fully validated candidate, preserves only a parseable live file as `.bak`, writes a sibling temp, then atomically renames it over the live config. Callers clone/apply/validate/save before swapping in-memory state, so a rejected change cannot partially mutate the running service.
- **`to_ui_settings`** projects `Config` into `eir_proto::UiSettings` for the tray app. Crucially it **never sends secrets** — API keys are reduced to `*_key_set: bool` flags via the local `set` closure (true iff present and non-empty). It surfaces provider, model, update-check model, effort, the three poll/decision intervals, channels/dirs, and `confidence_threshold` (plus the deprecated always-empty `base_url`/`api_key_set` wire-compat fields).
- **`apply_update(SettingsUpdate)`** applies a UI edit: blank/None secret fields **keep the stored value** (the `keep` closure), so the UI never needs to re-send keys it can't read back. Intervals are floored (`decision ≥10`, event-log `≥5`, wmi `≥30`); `confidence_threshold` is clamped to `0.50–0.95`; `effort` is `normalize_effort`'d to one of low|medium|high|xhigh|max or empty. `ApiProvider::parse` maps `claude_cli`/`openrouter`/`kilo_cli` (plus legacy aliases, including the removed gateway provider's `kilocode`/`kilo` tokens now folding into `kilo_cli`) and defaults anything else to Anthropic; the serde attributes on `ApiProvider` alias the removed `openai_compatible` token to Anthropic so a pre-0.17 config.toml still loads (its now-unknown extra keys are ignored by the toml loader — covered by `removed_provider_aliases_to_anthropic`; the `claude_cli` provider is first-class again as of v0.18, round-trip covered by `claude_cli_provider_round_trips_with_no_key_or_model`; the `kilocode` → `kilo_cli` alias is covered by `kilocode_provider_alias_loads_as_kilo_cli`).
- **Settings save hot-applies or restarts as needed.** In the loop (`main.rs:802`), an `UpdateSettings` message is validated by constructing an `AiClient` first (rejecting e.g. a keyless provider, reloading the prior config on failure so the service isn't bricked). A pure `settings_update_needs_restart` diff decides whether the changed fields require a process restart (only collector spawn parameters: event-log channels, event-log/WMI poll intervals, log directories). If so, `config::save` + `restart_self()`; if the helper fails to spawn the service stays alive on the old settings. Otherwise the new AI client, policy confidence threshold, and decision ticker are applied live without restart.

`AdvisorConfig` (`config.rs:26`) has its own `to_view`/`apply_view` with the same clamping discipline (threshold clamped `0.0–0.95`). It is config-only and not stored in the DB.

---

## Self-improvement: machine-pattern learning

> **Status: Phases 1–5 shipped (v0.15.0) — the self-improvement layer is complete.**
> (The one deferred item is the `RecurringFingerprint` sub-detector within Phase 3 —
> it overlaps `FixIneffective` and needs per-decision fingerprint identity.) Eir adapts to the specific
> machine it runs on instead of relying only on hardcoded rules: it learns self-updaters,
> failing update methods, ineffective service fixes, and actions the user keeps rejecting,
> and applies them at the updater's method order and the issue-analysis confidence gate —
> and surfaces what it has learned into the issue-analysis prompt. All effects are
> conservative (skip / deprioritise / capped confidence haircut) and security actions are
> never penalised (in the gate AND the prompt). The motivating case — Eir *discovering*
> that Discord self-updates and to stop fighting it — is learned from the audit history
> (`learn/`), with the hardcoded `SELF_UPDATING` seed kept only as a cold-start default.
> The rest of this section is the design of record; per-phase status is in the plan below.

### Principle

Eir already records every decision, execution outcome, and update attempt, and already
feeds recent execution feedback into the AI prompt. Self-improvement closes that loop:
detect patterns in the audit DB and adjust Eir's own behaviour — **auditable,
reversible, user-overridable, and bounded so a learned fact can only ever make Eir *more*
conservative** (skip, deprioritise, lower confidence, go idle), never more aggressive.

### Approach: deterministic core, AI as a later read-only advisor

A two-tier hybrid, shipped deterministic-first:

- **Tier 1 (Phases 1–4, pure Rust, zero AI cost):** SQL detectors over the existing
  audit tables form *learned facts*; Rust validates and persists them; four existing
  decision seams consult them. The AI is **not** in the learning write-path.
- **Tier 2 (Phase 5, optional):** a bounded AI labeller (under the existing advisor
  per-day count cap) may attach a plain-English explanation or a *strictly narrower*
  scope to a fact Rust already derived. It can never create, widen, change the kind of,
  or enable anything — exactly mirroring advisor mode (the AI advises; Rust gates).

### Data model — `migration 0008`

```sql
CREATE TABLE IF NOT EXISTS learned_facts (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  kind               TEXT    NOT NULL,   -- closed token mirrored by LearnedFactKind
  subject            TEXT    NOT NULL,   -- app_id | action_type | fingerprint | "app_id\u{1f}method"
  effect_json        TEXT    NOT NULL,   -- conservative-only Effect
  evidence_count     INTEGER NOT NULL,
  evidence_json      TEXT    NOT NULL,   -- compact provenance for the UI
  window_days        INTEGER NOT NULL,
  half_life_days     REAL    NOT NULL,
  first_seen_at      TEXT    NOT NULL,
  last_reinforced_at TEXT    NOT NULL,
  status             TEXT    NOT NULL,   -- active | expired | user_pinned | user_disabled
  source             TEXT    NOT NULL,   -- detector | ai_labelled
  ai_explanation     TEXT,
  UNIQUE(kind, subject)
);
CREATE TABLE IF NOT EXISTS approval_rejections (   -- closes the one real data gap
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  decision_id  INTEGER NOT NULL,
  action_label TEXT    NOT NULL,   -- format!("{action:?}")
  fingerprint  TEXT,
  rejected_at  TEXT    NOT NULL
);
```

Rust types: `enum LearnedFactKind { SelfUpdaterSuspected, MethodFailing, FixIneffective,
RecurringFingerprint, RejectedSignal }` (`from_token` returns `None` on unknown tokens —
a row from a newer build is skipped, never blindly trusted, like `Method::from_token`).
`enum Effect { Skip, DeprioritiseMethod(Method), ConfidencePenalty(f32), SuppressSignal }`
— a **closed, conservative-only** set; deliberately no variant enables an action, raises
confidence, unblocks a target, or adds a method. `effective_strength(now) = base *
0.5^(age_since_reinforced / half_life)` so an un-reconfirmed fact ages out and behaviour
self-heals.

### Detectors (deterministic, explicit thresholds)

| Kind | Quorum (30-day window) | Effect | Applied at |
|------|------------------------|--------|-----------|
| `SelfUpdaterSuspected{app}` | ≥3 distinct cycles where the app's update **timed out** (`exit_code == proc::TIMED_OUT`, or `category == NetworkTransient` with a "timed out" detail) **and 0 successes** | `Skip` | `check.rs::should_skip` (beside the `SELF_UPDATING` seed) |
| `MethodFailing{app,method}` | ≥3 failures + 0 successes for that method while **another** method succeeded for the app | `DeprioritiseMethod` | `orchestrator::heal()` — reorders `order`, never removes |
| `FixIneffective{action_type}` | ≥3 executions where `succeeded=1` but `improvement_score ≤ 0` | `ConfidencePenalty` (capped) | confidence haircut before `pol.evaluate` |
| `RecurringFingerprint{fp}` | same fingerprint reappears ≥3× after a fix was executed for it | `ConfidencePenalty` + AI context line | per-problem routing |
| `RejectedSignal{fp}` | user rejected the same fingerprint ≥3× with no intervening approval | `SuppressSignal` | `actionable_fingerprint` drops the part → cycle goes idle |

**Critical contract** (caught during design, against the real code): a hung choco/winget
INSTALL returns `exit_code == proc::TIMED_OUT (-4)` and `classify_error` maps "timed out"
to **`NetworkTransient`, not `InstallerFailed`** (`updater/domain.rs:337`). The
self-updater detector must key on `TIMED_OUT`/`NetworkTransient`, or it misses the Discord
case entirely. This is pinned by an integration test over a captured `TIMED_OUT` outcome.

**Always-excluded from suppression/haircut:** security action types (`firewall_enable`,
`defender_*`) and failed-service / security fingerprints — a learned fact must never
silence a real fault or weaken posture.

### Flow

DETECT (`learn::analyse(pool)` on already-collected data inside the existing updater cycle
and decision loop — no new threads, no AI) → VALIDATE (`validate.rs`: re-check quorum,
resolve the subject to a real installed app / known action type / live fingerprint, assert
the effect is in the conservative allow-set and does not contradict the loaded
`ExecutionPolicy`) → PERSIST (idempotent `INSERT … ON CONFLICT(kind,subject) DO UPDATE`
bumping `evidence_count`/`last_reinforced_at`) → APPLY (the four seams) → DECAY (each cycle
re-confirms or ages out; a `SelfUpdater` skip is periodically re-probed — one attempt every
~14 days — so a stale skip can lapse if the app is fixed upstream).

### Safety & governance

- Conservative by construction (the `Effect` set can only do *less*).
- Rust holds final authority end-to-end; `ConfidencePenalty` is capped (~0.15) and never
  applied to security actions.
- Decay + periodic re-probe self-heal; bounded formation (rolling window + quorum +
  cross-method/no-success guards) stops a bad-release week from sidelining a healthy app.
- Reversible + user-overridable with explicit precedence:
  **`user_disabled` > `user_pinned` > active detector fact > `SELF_UPDATING` seed**, and the
  existing per-app Ignore (`SetAppIgnore`) remains a hard override.
- Idempotent / restart-safe (same SQLite DB; additive `IF NOT EXISTS` migration).

### UI transparency

A **"What Eir has learned about this machine"** card (mirroring the App Updates history
card + its Clear pattern): each fact shows a plain-English summary, the why
(`evidence_count`/window), a strength/decay indicator, provenance, and per-fact
**Pin / Disable / Forget**. New `LearnedFactView` wire type + `Pin/Disable/Forget`
`UiMsg` variants (mirroring `UpdateAttemptRow` / `SetAppIgnore`). **No learned fact
changes behaviour materially before it is visible here with its reason** — silent
behaviour change is the trust risk, and the card is the answer.

### Phased plan

1. **Phase 1 — Learn the Discord case (subsume the hardcode). ✅ Shipped (v0.12.0).**
   `migration 0008` (`learned_facts`), `learn/` module (`SelfUpdaterSuspected` +
   `Effect::Skip`), the timeout-quorum detector, `should_skip` ORs in the learned lookup
   beside the kept seed. `analyse()` runs at the end of the updater cycle. Refinements
   from the adversarial review folded in: the timeout signal requires `category ==
   network_transient` so the native tamper-abort `-4` (`hash_mismatch`) is **not**
   mislabelled a self-updater; an **evidence-window re-probe** (`expire_unsupported_…`)
   expires a fact once its timeouts age out of the window, so a slow/large-app false
   positive self-corrects rather than skipping forever; and the App Updates **Clear**
   resets detector-learned facts (user pin/disable preserved). *Known limitation: a
   genuinely too-large/too-slow install (legitimately exceeding the 600s cap) reads the
   same as a self-updater hang; it is periodically re-probed and clearable, and Phase 2's
   finer signals refine it.*
2. **Phase 2 — Method preference + decay/re-probe. ✅ Shipped (v0.13.0).** `MethodFailing`
   → `DeprioritiseMethod` in `heal()` (stable-sort to the back, never removed); the
   evidence-window reconcile from Phase 1 generalised to every kind = decay/re-probe.
   Review fix: only method-attributable failure categories count — an integrity rejection
   (`hash_mismatch`/`signature_rejected`) never deprioritises the method that caught it.
3. **Phase 3 — Confidence + signal-noise learning; issue analysis uses learning.
   ✅ Shipped (v0.13.0).** `migration 0009` `approval_rejections` (written on
   `Approve{approved:false}`); `FixIneffective` (service fixes that never reduce failed
   services — only when the type *never* helps) and `RejectedSignal` (exact action label)
   → a **capped confidence haircut into the existing policy gate**, never for security
   actions; a read-only "what Eir has learned" section injected into the issue-analysis
   prompt (base + advisor-escalation), with the security carve-out applied to the prompt
   too. `analyse_issues()` runs each decision cycle. *`RecurringFingerprint` deferred —
   overlaps `FixIneffective` and needs per-decision fingerprint identity.*
4. **Phase 4 — UI transparency + user override. ✅ Shipped (v0.14.0).** `LearnedFactView`
   in the status broadcast; a "What Eir Has Learned" tray card listing each fact with its
   evidence and **Pin / Disable / Forget** (`UiMsg::SetLearnedFact`); precedence enforced
   in the store (`user_disabled`/`user_pinned` survive reinforcement and the
   evidence-window reconcile; a disabled self-updater fact stops being applied).
5. **Phase 5 — AI labeller (Tier 2). ✅ Shipped (v0.15.0).** `learn/label.rs` gives a
   not-yet-explained fact a one-sentence, human-readable explanation for the UI card. The
   AI is **read-only**: it returns explanation TEXT only (sanitised, ≤200 chars) — it
   never creates a fact or changes its kind/subject/effect, and `source` stays `detector`
   so the fact keeps its decay/clear lifecycle. **Bounded** by an `ai_label_attempted_at`
   marker (migration 0010): each fact is attempted at most once, so total AI calls ≤ the
   number of facts and steady state is zero (review fix — previously an unusable reply
   re-fired a call every cycle and starved the queue). Scoped to a deterministic-only
   narrowing was not needed since the AI never touches structure.

### Superseding the hardcode

In Phase 1 `SELF_UPDATING` is demoted from "the rule" to a **cold-start seed**:
`should_skip()` becomes `SELF_UPDATING.contains(base) || learned::is_self_updater(base) ||
cfg.ignored…`. Once Phase 1 has run a full 30-day window on a machine with Discord
installed and a `SelfUpdaterSuspected{discord}` fact has demonstrably formed from real
attempts, the seed is redundant and can be reduced to an empty slice (kept as the typed
seam) or removed. Do **not** remove it pre-emptively — that regresses cold-start for the
one app already known to behave this way.

### Residual implementation notes

- The weighted resource `improvement_score` remains a prompt fallback; targeted service
  resolution is the authoritative input to `FixIneffective`.
- `RecurringFingerprint` remains deferred; it overlaps `FixIneffective` and still needs
  per-decision fingerprint identity before it can be applied cleanly.


---

## Current limitations and roadmap

This section is authoritative for v0.33.0. [PLAN.md](PLAN.md) holds the operational
burn-in checklist and ordered v0.34 work; [CONTEXT.md](CONTEXT.md) records release history.

### Verification baseline

- The v0.33 local gate passed formatting, clippy, 299 tests, JavaScript/version checks,
  release compilation, and the NSIS build.
- Release CI gates the exact commit with a fresh LocalSystem service install, a real
  protocol-v2 correlated command, signed Tauri/NSIS artifacts, and a standalone-app smoke.
- The packaged v0.33 WebView was exercised against the installed v0.32 service: version
  skew and the unsupported provider-test capability rendered correctly, and a live Codex
  Ask round-trip completed. The installed v0.33 LocalSystem provider test remains a burn-in
  check, not a claimed live result.

### Delivery and persistence

- CI proves a fresh install, not an upgrade from the previous release with representative
  config and database state. An exact-SHA upgrade gate is the first v0.34 priority.
- Config has no explicit schema version. Serde defaults load older files, but saving
  serialises known fields and can discard unknown keys from a newer config.
- A corrupt audit database fails closed instead of being deleted automatically; recovery is
  intentionally manual to avoid destroying approvals or history.

### Pipe and UI

- The pipe accepts one active-interactive-session client at a time; concurrent tray clients
  are unsupported.
- Protocol v2 correlates command results. During service/UI skew, protocol v1 retains the
  flattened command shape and can only return a neutral queued result.
- Long-running scans and updater cycles confirm that work started, then report completion
  through status/activity. The frontend renders a local cache on a two-second poll.

### Signals

- Event Log polling and the in-memory drain buffer each cap bursts at 100 records per
  channel; excess older records are not replayed.
- Automatic file discovery is one directory level deep, limited to recent files, and reads
  the newest 64 KiB per change. Explicit watched directories bypass discovery, not the tail
  limit.
- Disk capacity covers the system drive. CPU and some security/disk/network probes depend on
  bounded PowerShell or Windows APIs; probe failures retain last-good values and surface
  source errors/freshness instead of publishing healthy zeroes.

### AI and learning

- Anthropic cost uses a list-price estimate; subscription CLI “cost” is an equivalent-value
  estimate, so totals are visibility rather than billing.
- CLI providers depend on the active user's installed binary layout and logged-in session.
  Unsupported model/effort combinations surface as call errors rather than being fully
  prevalidated.
- Model JSON has a bounded extraction fallback, and decision/feedback context windows remain
  fixed and small.
- Verified targeted outcomes currently drive effectiveness learning only for service
  actions. Other adapters verify effects for execution success, but do not yet emit
  structured learned outcomes. Learning remains conservative-only: it can skip,
  deprioritise, or reduce confidence, never expand authority.

### Execution and undo

- Registry reset has crash-durable, exact-type undo guarded by a comparison with the value
  Eir applied. Other action families do not yet have durable undo receipts; startup and task
  toggles are the next narrow candidates.
- Rate limits and circuit breakers bound repeated failures, but there is no success-driven
  auto-approval promotion.

### App updater

- A native install's expected publisher is currently AI-sourced, so publisher matching is a
  tripwire against mismatch, not a durable vendor pin. v0.34 should anchor it in signed
  installed software or a curated local mapping.
- Scoop stays disabled under LocalSystem; Microsoft Store updates remain best-effort because
  entitlement and installs are user-scoped.
- Unmanaged AI checks rotate the stalest 20 apps per cycle. Recorded failures or deferred
  coverage block a clean-cycle claim, but an enabled package manager that is unavailable
  can be omitted when another source is usable; “clean” is not proof that every configured
  manager was available.
- Version normalisation still has edge cases around prereleases and vendor-specific version
  shapes. Native installer execution under the released LocalSystem service remains part of
  normal-use burn-in.

No new repair authority is planned until these trust-loop gaps are closed.
