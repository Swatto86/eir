<!--
  LIVING DOCUMENT — keep this current with every architectural change.
  Update the relevant section (and the "Last updated" line) in the same commit that
  changes behaviour. Sections below were machine-mapped from the source and then
  curated; treat the code as ground truth and correct this doc when they diverge.
-->

# Eir — Architecture & Design

**Last updated:** 2026-06-26 · **Release:** v0.16.0

Eir is an autonomous Windows system-repair agent: it watches a machine's health,
uses an AI model to diagnose problems, and applies least-destructive fixes —
auto-running reversible whitelisted repairs and queuing anything disruptive for
approval. It also keeps installed apps up to date unattended.

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

**The decision cycle** (default every 10 min, `decision_interval_secs`): collect
signals → compute an *actionable fingerprint* and only call the AI when something
actionable changed (plus a periodic heartbeat) → AI returns structured problems each
with a confidence and a proposed `FixAction` → policy gates each (auto-execute /
require-approval / block) → reversible whitelisted fixes at/above the confidence
threshold run on an **off-loop executor worker**; disruptive/irreversible ones queue
for approval. Every decision, execution, approval, and update attempt is persisted to
the audit DB — which is the substrate the self-improvement layer learns from (Phase 1
shipped; see [Self-improvement](#self-improvement-machine-pattern-learning)).

## Table of contents

- [Workspace, build & delivery pipeline](#workspace-build--delivery-pipeline)
- [Pipe protocol & tray UI](#pipe-protocol--tray-ui)
- [Service decision loop, state & off-loop executor](#service-decision-loop-state--off-loop-executor)
- [Signal sources](#signal-sources)
- [AI layer & prompts](#ai-layer--prompts)
- [Executor, policy, safety & explanations](#executor-policy-safety--explanations)
- [Autonomous app updater](#autonomous-app-updater)
- [Persistence, audit DB & the existing feedback loop](#persistence-audit-db--the-existing-feedback-loop)
- [Self-improvement: machine-pattern learning](#self-improvement-machine-pattern-learning)
- [Known limitations & backlog](#known-limitations--backlog)

---


## Workspace, build & delivery pipeline

Eir is a single Cargo workspace (`resolver = "2"`) with three crates, plus a static, hand-written frontend and a Tauri-driven NSIS delivery pipeline. There is **no JavaScript toolchain** — no `package.json`, no bundler, no `npm` step anywhere — which shapes the entire build.

### Crate layout & layering

`Cargo.toml` (repo root) declares `members = ["eir-proto", "eir-svc", "eir-ui"]`. Mapping to the layering model (per `README.md` "Project layout", confirmed in each `Cargo.toml`):

| Crate | Layer | Binary | Responsibility |
|-------|-------|--------|----------------|
| `eir-proto` | shared/contract | (lib) | Wire types for the UI↔service named-pipe protocol (serde, snake_case). Pure types, no I/O. Depended on by both other crates (`eir-proto = { path = "../eir-proto" }`). |
| `eir-svc` | infrastructure/service | `eir-svc` (`src/main.rs`) | LocalSystem Windows service: signal collection, AI client, policy, execution, autonomous updater, SQLite audit DB. Heavy `windows` 0.58 feature set. |
| `eir-ui` | presentation/composition root | `eir` (`src/main.rs`) | Tauri v2 tray app. Wires the system together and renders status/approvals/updates. Deps: `tauri` 2 (`tray-icon`), `tauri-plugin-autostart` 2, `tauri-plugin-updater` 2, `tokio` (full), `image` (png), tracing. `build-dependencies`: `tauri-build` 2. |

All three crates are versioned in lockstep — currently `0.11.3` in every `[package] version` (`eir-proto/Cargo.toml:3`, `eir-svc/Cargo.toml:3`, `eir-ui/Cargo.toml:3`).

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
1. `actions/checkout@v6`
2. `dtolnay/rust-toolchain@1.95.0` with `rustfmt, clippy`
3. `swatinem/rust-cache@v2`
4. **Format**: `cargo fmt --all --check`
5. **Stage service binary**: runs `build-svc.ps1` — required because `eir-ui`'s `tauri_build` validates bundle resources (incl. the gitignored `bin/eir-svc.exe`) during a bare `cargo clippy`/`cargo test`; without staging, clippy/test would fail on the missing resource.
6. **Clippy**: `cargo clippy --all-targets -- -D warnings` (`--all-targets`, not `--lib`, per the user's rule).
7. **Test**: `cargo test --workspace`.
8. **Full Tauri build**: `tauri-apps/tauri-action@v0` with `projectPath: eir-ui`, signing keys from secrets (`TAURI_SIGNING_PRIVATE_KEY`, `..._PASSWORD`), **no `tagName`** so it builds and signs but publishes nothing. This re-runs `beforeBuildCommand` and produces/signs the real bundle so a broken bundle can't pass green.

Note: step 5 stages the binary, then step 8's full build stages it again via `beforeBuildCommand` — redundant but intentional (steps 6–7 need it before the bundle build runs).

The green CI gate is the only pre-release bar (matches the user's "no manual/live-test gate" policy).

### Tag-driven signed release (`.github/workflows/release.yml`)

Triggers: `push` of tags matching `v*`. `permissions: contents: write`. Single job `release` on `windows-latest`:
1. `actions/checkout@v6`
2. `dtolnay/rust-toolchain@1.95.0` (no components — release doesn't lint)
3. `swatinem/rust-cache@v2`
4. `tauri-apps/tauri-action@v0` with `projectPath: eir-ui`, `tagName: ${{ github.ref_name }}`, `releaseName: Eir ${{ github.ref_name }}`, a templated `releaseBody`, `releaseDraft: false`, `prerelease: false`. Env: `GITHUB_TOKEN` + the two `TAURI_SIGNING_*` secrets.

Because `tagName` is set, `tauri-action` builds, signs, and **publishes** a GitHub release with the NSIS installer (`Eir_<version>_x64-setup.exe`), `latest.json`, and the `.sig`. The signing keypair is minisign; the public key is embedded in `tauri.conf.json` `plugins.updater.pubkey` (base64 minisign public key).

### Self-update wiring (single rolling release)

`tauri.conf.json` `plugins.updater.endpoints` is a single URL:
`https://github.com/Swatto86/eir/releases/latest/download/latest.json`. It points at the **`/latest/`** redirect, so the installed app always fetches whichever release is newest — the single-rolling-release model. `createUpdaterArtifacts: true` ensures `latest.json` + `.sig` ship beside the installer. The unauthenticated `/latest/download/` fetch requires the release repo to be public.

The NSIS install hooks (`eir-ui/installer-hooks.nsh`, wired via `bundle.windows.nsis.installerHooks`, `installMode: perMachine`) make self-update actually work over a running service:
- **PREINSTALL**: `sc stop EirSvc` + `Sleep 5000` — must stop the service before files are written, because Windows can't replace `eir-svc.exe` while it runs (the comment notes this is "what broke auto-updates").
- **POSTINSTALL**: stop + `eir-svc.exe uninstall` (tear down prior registration), seed `config.toml` from `config.toml.example` on first install then delete the template, then `eir-svc.exe install` + `sc start EirSvc`.
- **PREUNINSTALL**: `sc stop EirSvc` + `eir-svc.exe uninstall`.
- **POSTUNINSTALL**: empty.

`bundle.windows.allowDowngrades: false`; targets `["nsis"]` only; main window `visible: false`. Manual launches show/focus it during Tauri setup; Windows-login autostart passes `--hidden` so it stays in the tray.

### Version-bump locations

A release version lives in **four** places that must move together:
- `eir-ui/tauri.conf.json` → `"version"` (drives installer filename, About, updater compare).
- `eir-proto/Cargo.toml`, `eir-svc/Cargo.toml`, `eir-ui/Cargo.toml` → `[package] version`.
- `Cargo.lock` must be re-synced (it is present and tracked, 152 KB).

There is **no `package.json`**, so the WattMail blueprint's "bump `package.json`" step does not apply here. The release-commit `[release]` marker convention is visible in git history (e.g. `0752c08 ... (v0.10.2) [release]`).

### Build/release control flow (summary)

Developer bumps the 3 `Cargo.toml` versions + `tauri.conf.json` + `Cargo.lock`, commits with `[release]`, pushes to `master` (CI gate runs), then pushes tag `vX.Y.Z` → `release.yml` builds the signed bundle and publishes the rolling GitHub release → installed clients poll `releases/latest/download/latest.json`, verify the minisign `.sig` against the embedded pubkey, and self-update (NSIS hooks stop/replace/restart `EirSvc`).

## Pipe protocol & tray UI

The UI subsystem is a thin Tauri tray app (`eir-ui`) that talks to the LocalSystem service (`eir-svc`) over a single Windows named pipe, `\\.\pipe\EirSvc`. All wire types live in the shared `eir-proto` crate so both ends serialize/deserialize the same shapes. The service owns all state; the UI is a stateless renderer that polls a locally-cached snapshot and sends fire-and-forget commands.

### Transport & framing

- **Pipe name**: `eir_proto::PIPE_NAME = r"\\.\pipe\EirSvc"` (`eir-proto/src/lib.rs:3`), used by both server (`pipe_server.rs:55`) and client (`pipe_client.rs:16`).
- **Framing**: newline-delimited JSON ("JSON lines"). Each direction writes one `serde_json` object per line terminated with `\n`; readers use `BufReader::read_line` and `serde_json::from_str` on the trimmed line. Pipe mode is **byte stream** (`PipeMode::Byte`, `pipe_server.rs:87`), not message mode — framing is purely the newline.
- **Bidirectional, split**: on both ends the connected pipe is `tokio::io::split` into an independent reader and writer. The service runs the writer as a spawned task and the reader inline (`pipe_server.rs:126-182`); the client runs both as `async` blocks joined by `tokio::select!` (`pipe_client.rs:63-113`).

### Wire types (`eir-proto/src/lib.rs`)

Two tagged enums carry everything (`#[serde(tag = "type", rename_all = "snake_case")]`):

- **`ServiceMsg`** (service → UI), one variant: `Status(StatusPayload)` (`lib.rs:269-273`).
- **`UiMsg`** (UI → service) (`lib.rs:276-304`): `Approve { id: u64, approved: bool }`, `TogglePause`, `UpdateSettings(Box<SettingsUpdate>)`, `ClearProblems`, `ClearExecutions`, `RunUpdatesNow`, `ClearUpdateHistory`, `UpdateUpdaterSettings(Box<UpdaterSettingsUpdate>)`, `SetAppIgnore { id: String, ignore: bool, note: String }`, `SetAdvisorSettings(Box<AdvisorSettingsUpdate>)`. (`Box` keeps the enum small since the settings variants are large.)

**`StatusPayload`** (`lib.rs:5-34`) is the single snapshot the UI renders, carrying: `status` (string state machine value), `paused`, `cpu`/`memory`/`disk` (`f32` percentages), `failed_services`, `last_analysis`, `recent_problems: Vec<ProblemSummary>`, `recent_executions: Vec<ExecutionSummary>`, `pending_approvals: Vec<ApprovalInfo>`, `error: Option<String>`, `usage: Option<UsageSummary>`, `settings: Option<UiSettings>`, `updater: Option<UpdaterStatus>`, `advisor: Option<AdvisorStatus>`. Derives `Default` so the channel can be seeded empty.

Supporting types:
- **`ApprovalInfo`** (`lib.rs:207-240`): `id: u64`, `diagnosis`, `root_cause`, `confidence: f32`, `action` (debug render of the fix), `reason` (policy verdict), `side_effects`, `undo_instructions`, plus the trust-critical deterministic fields `action_summary`, `target`, `target_details`, `reversible: bool`, `created_at: i64`. The doc comment notes `action_summary` is "derived from the action type, not the AI, so it can be trusted."
- **`ProblemSummary`** (`lib.rs:242-256`): `diagnosis`, `confidence`, `action`, `blocked`, `auto_executed`, `reason: Option<String>`, `at: i64`.
- **`ExecutionSummary`** (`lib.rs:258-266`): `action`, `success`, `preview`, `at: i64`.
- **`UpdaterStatus`** (`lib.rs:73-95`): `enabled`, `running`, `phase`, `last_run`/`next_run` (unix secs), `last_cost_usd`, `notes: Vec<String>`, `apps: Vec<UpdaterAppRow>`, `recent: Vec<UpdateAttemptRow>`, `settings: UpdaterSettingsView`. `UpdaterAppRow` carries per-app `state` ("verified"|"installed"|"failed"|"skipped"), `method`, `signature`; `UpdateAttemptRow` is history.
- **`AdvisorStatus`** (`lib.rs:38-51`): `enabled`, `escalated`, `escalation_model`, `reason`, `spent_today_usd`, `settings: AdvisorSettingsView`.
- **`UiSettings`** / **`UsageSummary`** plus the `*Update` mirrors (`SettingsUpdate`, `UpdaterSettingsUpdate`, `AdvisorSettingsUpdate`) that flow back as `UiMsg` payloads.

**Backward-compat invariant**: every field added after the original protocol is annotated `#[serde(default)]` (e.g. `pending_approvals`, `updater`, `advisor`, `effort`, the deterministic `ApprovalInfo` fields, all `at` timestamps). This lets an older service or UI decode a newer payload without error — a deliberate forward/backward-compatibility design across version skew.

**Secret-handling invariant**: `UiSettings` never carries secret values, only booleans (`openrouter_key_set`, `anthropic_key_set`, `api_key_set`) so the UI shows "configured" without exposing keys (`lib.rs:144-170`). Inbound `SettingsUpdate` uses `Option<String>` for secrets where `None` = "unchanged" and a non-empty value replaces the stored secret (`lib.rs:172-192`); the JS sends `null` to preserve (`main.js:552-554`).

### Named-pipe security / ACL model (`pipe_server.rs:16-48`)

A pipe created by a LocalSystem service defaults to granting only SYSTEM + Administrators, so a non-elevated, medium-integrity UI would get "Access is denied." `build_pipe_security_descriptor()` builds an explicit descriptor from the SDDL string:

```
D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)S:(ML;;NW;;;ME)
```

- **DACL**: SYSTEM (`SY`) and Administrators (`BA`) get full control (`GA`); Authenticated Users (`AU`) get read **and** write (`GRGW`) — write is required so the UI can send commands/approvals, not just read status.
- **SACL mandatory label**: `S:(ML;;NW;;;ME)` sets a **Medium** integrity label with the no-write-up (`NW`) policy. Without this the pipe inherits the creator's System integrity and Windows' no-write-up rule would silently let the medium-integrity UI *read* but block its *writes* (Approve/Reject/Pause). Labelling the pipe Medium permits the UI's writes while still blocking Low-integrity (sandboxed) processes.

Implementation details: the descriptor is **intentionally leaked** and returned as a `usize` (not a raw pointer) so it can cross the listener's `.await` points (a raw pointer is not `Send`); `pipe_server.rs:32,47`. The `SECURITY_ATTRIBUTES` is constructed inside a block that ends before the first `.await`, so the non-`Send` pointer is never held across an await (`pipe_server.rs:83-104`), and the pipe is created via `create_with_security_attributes_raw`. If descriptor construction fails the server falls back to a default-ACL pipe and warns (`pipe_server.rs:77-79, 102`).

### Service side: listener & broadcast (`pipe_server.rs`)

- `spawn()` creates a `watch::channel<StatusPayload>` (status fan-out) and an `mpsc::channel<UiMsg>` (size 8, command intake), returns a `PipeServer { status_tx }` plus the `UiMsg` receiver, and spawns `listener_task` (`pipe_server.rs:54-69`).
- `PipeServer::broadcast_status()` just does `status_tx.send()` — the rest of the service pushes a fresh snapshot here whenever state changes (`pipe_server.rs:187-191`).
- **Listener loop**: creates a pipe instance (`first_pipe_instance` only on the first iteration), `connect().await`s a client, then splits it. The **writer task** sends the current `watch` value immediately on connect (snapshot-on-connect), then resends on every `status_rx.changed()` (`pipe_server.rs:131-152`). The **reader loop** reads lines, deserializes `UiMsg`, and forwards each on the `mpsc` to the decision loop with `ui_cmd_tx.send(msg).await` (`pipe_server.rs:157-180`). Bad messages are logged and skipped, not fatal.
- **Single-consumer design**: the listener handles one connection at a time; on disconnect (`read_line` returns `Ok(0)` EOF) it aborts the writer task and loops back to accept the next client (`pipe_server.rs:182-184`). There is no concurrent multi-client support.
- **Critical invariant**: Approve/Reject are *not* resolved in the pipe layer — all `UiMsg`, including approvals, are forwarded to the decision loop, "which owns the persistent approval queue" (`pipe_server.rs:166-167`). Approvals persist across cycles and service restarts (per `StatusPayload.pending_approvals` doc, `lib.rs:16-18`), so an approval never expires out from under the user.

### UI side: pipe client (`eir-ui/src/pipe_client.rs`)

- `run(status: SharedStatus, cmd_rx)` loops forever, reconnecting on disconnect with a 5s backoff. `SharedStatus = Arc<Mutex<StatusPayload>>` is the locally-cached snapshot the Tauri command reads (`pipe_client.rs:11,15-37`).
- On disconnect it overwrites the cached status to `"ServiceDisconnected"` with an actionable error string telling the user to install/start the service (`pipe_client.rs:24-33`).
- `connect_and_run` opens the client with retry handling: `ERROR_FILE_NOT_FOUND` (os err 2) → bubble up (pipe not created yet, triggers reconnect); `ERROR_PIPE_BUSY` (231) → 50ms retry; other errors → propagate (`pipe_client.rs:45-59`).
- **Read and write are separate loops joined by `select!`** (`pipe_client.rs:71-112`). The read loop deserializes `ServiceMsg::Status` and replaces `*status.lock()` wholesale. The write loop drains `cmd_rx` and writes each command as a JSON line + flush. A documented design note explains *why* they're separate: a previous single-`select!` design cancelled the in-flight `read_line` on every command send, and `read_line` is not cancellation-safe, corrupting the status stream and starving writes (`pipe_client.rs:66-70`).

### Tauri command surface (`eir-ui/src/main.rs`)

Managed state: `SharedStatus` (the cached payload) and `UiCmdTx(mpsc::Sender<UiMsg>)`. The `main()` wiring creates the channel (size 16), seeds the cache to `"Connecting"`, spawns `pipe_client::run`, and registers 13 commands (`main.rs:239-251, 335-349`).

Commands (`main.rs:28-112`, plus `util.rs`):
- `get_status` — **synchronous**, returns a clone of the cached `StatusPayload`. This is the UI's only read path; it never hits the pipe directly.
- `decide_approval { id, approved }` → `UiMsg::Approve`.
- `toggle_pause` → `UiMsg::TogglePause`.
- `clear_problems` / `clear_executions` → `ClearProblems` / `ClearExecutions`.
- `update_settings(SettingsUpdate)` → `UpdateSettings`.
- `run_updates_now` → `RunUpdatesNow`.
- `clear_update_history` → `ClearUpdateHistory`.
- `set_updater_settings(UpdaterSettingsUpdate)` → `UpdateUpdaterSettings`.
- `set_app_ignore { id, ignore, note }` → `SetAppIgnore`.
- `set_advisor_settings(AdvisorSettingsUpdate)` → `SetAdvisorSettings`.
- `get_autostart_enabled` / `set_autostart_enabled { enabled }` — UI-local commands backed by `tauri-plugin-autostart`; they never cross the service pipe.
- `util::gbp_per_usd` (USD→GBP rate via a hidden PowerShell `Invoke-RestMethod`, with a `0.79` offline fallback) and `util::open_url` (validates `http(s)://` then `Start-Process`) — UI-local helpers, not pipe traffic (`util.rs`).

Every service-facing command is `async`, sends one `UiMsg` on `UiCmdTx`, and maps a send error to `Err(String)`. Those commands are **fire-and-forget**: success means "queued to the pipe writer," not "applied by the service." The UI observes the effect only on the next polled snapshot. `get_status`, the autostart commands, and `util::*` are UI-local and do not use the pipe.

### Tray (`main.rs:114-323`)

- Tray icon is built from an embedded 128px PNG (`ICON_PNG`), recoloured per status (`status_accent` maps states to RGBA tints: green=Active/untouched, amber=Warning, orange=PendingApproval, blue=Executing, red=Error/ServiceDisconnected, grey=other) and Lanczos3-downsampled to 32px (`make_icon`, `recolor`, `decode_icon`).
- Start-with-Windows is registered by `tauri-plugin-autostart` with the app name `Eir` and the `--hidden` argument. The UI preference is stored in Tauri's app config directory as `ui-preferences.json`, defaults to enabled on first run, and is synced to the OS startup entry during Tauri setup. This separate preference prevents a user-disabled startup entry from being re-enabled on the next manual launch.
- A background task polls the cached status every 500ms and only repaints the tray icon/tooltip when `status` changes (`main.rs:311-323`).
- Tray menu: Open Status / Pause Monitoring / Quit. "Pause" sends `UiMsg::TogglePause` directly via the cloned `UiCmdTx`; left-click shows the window (`main.rs:259-302`).
- **Close-to-tray**: `WindowEvent::CloseRequested` hides the window and `prevent_close()`s; the service keeps running; Quit fully exits (`main.rs:327-334`). The JS also redundantly intercepts `onCloseRequested` (`main.js:54-57`).
- Self-update: `tauri-plugin-updater` checks 15s after launch then every 6h, and on a newer signed release downloads/installs (NSIS, which elevates and updates the service too) and relaunches (`main.rs:203-232`).

### Polling & rendering (`ui/index.html`, `ui/main.js`)

- **Poll cadence**: `refresh()` calls `get_status` and re-renders, on `setInterval(..., 2000)` — a 2s poll of the *local cache* (no pipe round-trip per poll). `gbp_per_usd` is fetched once at load (`main.js:636-640`).
- `refresh()` (`main.js:202-264`) maps `status.status` to a header dot colour (`STATUS_COLORS`), renders the metric bars (colour-coded by threshold in `barColor`), failed-services chips, the model labels (`analysisLabel`/`updateCheckLabel`), then delegates to `renderApprovals`, `renderAiNow`, `renderActivity`, `renderUsage`, `renderUpdater`.
- **XSS hygiene**: all service-supplied strings go through `esc()` / `escAttr()` before insertion into `innerHTML` (`main.js:155-163`); applied consistently across approvals, activity, updater rows, and service chips.
- **Activity feed** merges `recent_problems` + `recent_executions` into one list sorted by `at` descending, with emoji/tag per kind (`activityItems`, `main.js:301-329`).
- **Settings** are a modal populated from `lastStatus.settings`/`.updater.settings`/`.advisor.settings` plus the UI-local autostart command. Four independent save buttons map to `set_autostart_enabled` (applies immediately, no service pipe), `update_settings` (warns it restarts the service ~15s), `set_updater_settings`, and `set_advisor_settings` (both apply live). Inputs are converted (e.g. confidence % → 0–1 float, hours → seconds) before sending (`main.js`).
- Collapsed-card state is persisted in `localStorage` (`main.js:606-633`).

### Clear / Approve / Ignore / Update-now flows

- **Approve / Reject**: a delegated click handler on `#approvals` parses the card's `data-id`, **disables both buttons** to prevent double-submit, and calls `decide_approval(id, approved)`; on error it re-enables them (`main.js:271-280, 456-462`). The command → `UiMsg::Approve` → pipe → decision loop, which resolves it against the persistent queue. The card disappears on the next poll once the service drops it from `pending_approvals`.
- **Pause**: header button (and tray menu) → `toggle_pause` → `UiMsg::TogglePause`; the button label flips Pause/Resume based on `status.paused` (`main.js:228-229, 266-269`).
- **Clear (Activity)**: one button fires both `clear_problems` and `clear_executions` then `refresh()`s (`main.js:601-604`). **Clear (Updates)** → `clear_update_history` (clears last cycle + persisted attempts).
- **Ignore (per app)**: delegated click on `#updater-apps` sends `set_app_ignore { id, ignore:true, note:"" }` and optimistically dims the row (`main.js:472-478`).
- **Update now**: `#upd-now` → `run_updates_now` → `UiMsg::RunUpdatesNow`. The button is disabled when the updater is `running` or `!enabled`, because "the service ignores a manual run unless the updater is enabled" — the UI mirrors that gate rather than enforcing it (`main.js:369-375, 412-415`).

## Service decision loop, state & off-loop executor

The heart of `eir-svc` is a single async supervisory loop in `eir-svc/src/main.rs` that owns all mutable service state, collects signals, gates AI-proposed fixes through policy, and dispatches actual execution to a separate worker so the loop never blocks. All paths cite `eir-svc/src/main.rs`.

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
6. **AI client init is non-fatal** (646-656): a bad AI config sets `status="Error"` + an actionable error and leaves `ai = None`, but the service keeps running so Settings stays usable.
7. Spawns signal collectors: `event_log`, `file_watch` (after `discover_watch_dirs`), `wmi`. Sleeps 5 s to let them warm up.
8. Restores `usage_summary` and `load_pending_approvals` from the DB, then settles `status = resting_status(&st)` and broadcasts.
9. Sets up the decision `ticker` (`cfg.monitoring.decision_interval_secs`), channels, and the executor worker (below).

### The `tokio::select!` decision loop (lines 729-1378)

The outer `tokio::select!` races the main loop future against the `shutdown` future (1375-1377 logs and exits). Inside the `loop {}`, an inner `tokio::select!` (735-974) multiplexes six sources; the first five `continue` immediately so commands/outcomes are serviced without waiting for a decision tick:

1. `ticker.tick()` — falls through to the decision body below.
2. `update_done_rx` (737-757) — an update cycle finished: clears `updater_running`/`updater.running`, records `last_run`/`last_cost_usd`/`notes`/`apps`, sets `phase="idle"`, refreshes history, recomputes `next_run`.
3. `update_progress_rx` (758-767) — coarse live phase label; **guarded on `updater_running`** so a straggling message can't overwrite the `idle` a just-finished cycle set.
4. `exec_done_rx` (768-786) — a fix finished on the worker: removes label from `in_flight`, pushes an execution + a `auto_executed=true` problem entry, settles `resting_status`. Explicitly **does not touch `st.error`** so an execution outcome can't wipe an unrelated AI/connection error.
5. `ui_rx` (787-973) — UI commands (see below).
6. (no match / tick) — proceeds to the per-cycle body.

#### Per-cycle body (after a tick, lines 975-1372)

- `cycle_count += 1`; every 20 cycles re-discovers log dirs and feeds new ones to `file_watch` via `dir_update_tx` (978-996).
- **Scheduled updater** (999-1015): if `enabled && !paused && !updater_running && (last_run==0 || elapsed >= interval)`, sets running flags, `phase="checking…"`, and `spawn_update_cycle`.
- `if st.paused { continue; }` (1017-1019) — paused skips analysis but **still ran the updater gate above** (which itself checks `!paused`, so paused also suppresses updates).
- Collects `decision_history` (last 5), builds a `SignalSnapshot` (event log + file changes drain + wmi current), updates live metrics, broadcasts.
- `let Some(ai) = ai.as_ref() else { continue }` (1054-1056) — no provider configured: keep collecting/serving UI, skip analysis.
- Updates feedback after-states and pulls `feedback::recent_summary`.
- **Idle-skip gate** (1071-1088): computes `actionable_fingerprint`; `changed = fingerprint.is_some() && != last_fingerprint`; `heartbeat_due` if `last_analysis_at` is `None` (forces baseline) or elapsed ≥ `ANALYSIS_HEARTBEAT` (6 h). If `!changed && !heartbeat_due`, settle `resting_status`, clear error (unless paused), broadcast, `continue` — saves AI spend on unchanged idle cycles.
- **Claude analysis** (1091-1112): `ai.analyze(...)` returns `(decision, usage)`; usage is logged and the summary recomputed; on error sets `status="Error"`/`error` and continues. After success, `last_fingerprint`/`last_analysis_at` are updated and `st.last_analysis` set.
- **Advisor escalation** (1122-1179): resets day-counters at the UTC boundary, builds an `AdvisorStatus`, and if `should_escalate` returns `Some(reason)`, increments `advisor_escalations_today` **before** the call (so a failing escalation can't retry every cycle and defeat the cap), re-analyses via `ai.analyze_with(model, effort)`, adds usage cost to `advisor_spent_today`, and on success replaces `claude_decision`/`last_analysis` and marks `adv.escalated`.
- `audit::log_decision` returns `decision_id` (1181-1193). Logs `safety::success_rate` and warns if < 0.85.
- **Per-problem routing** (1206-1355): for each problem, `parse_fix_action()`; an unparseable fix becomes a blocked problem (`blocked=true`). Otherwise `pol.evaluate(&action, confidence)`:
  - `Block(reason)` → push blocked problem.
  - `AutoApprove` → check `safety::rate_limited`; if not rate-limited and label not in `in_flight`, insert label, send an `ExecJob` (reason `None`) to the executor, set `status="Executing"`.
  - `RequireApproval(reason)` → **non-blocking**: skip if label already in `pending` or `in_flight` (prevents a double-run), else build `ApprovalInfo` (via `explain::explain`/`target_details`), `insert_pending_approval` (DB), push to `st.pending` with the DB row id, set `status="PendingApproval"`. On DB failure it surfaces the finding as a blocked problem so it isn't lost.
- Finally computes a tray status with precedence `Paused > PendingApproval > Executing > Warning(problems_found) > Active` (1357-1372) and broadcasts.

#### UI command handlers (`ui_rx`, lines 787-971)

- `TogglePause` → flip `paused`, resettle status.
- `ClearProblems` / `ClearExecutions` → clear the respective deque.
- `UpdateSettings` → `apply_update`, **validate by constructing a fresh `AiClient`**; on failure reject and reload config from disk (never restart into a bricked provider); on success save and `restart_self()` then `return` (provider settings require a restart).
- `Approve { id, approved }` → find/remove the pending item and `delete_pending_approval`. If approved and not already `in_flight`, insert label and send an `ExecJob` (reason `"approved by user"`); if approved but already in-flight, log and skip re-run; if rejected, push a `rejected by user` problem. Always resettle status + broadcast.
- `RunUpdatesNow` → **gated on the same controls as the scheduled run** (`enabled && !paused && !updater_running`) — the pipe is writable by any authenticated user, so a manual run must not override admin state.
- `ClearUpdateHistory`, `UpdateUpdaterSettings`, `SetAppIgnore`, `SetAdvisorSettings` → applied **live, no restart** (unlike provider settings), each persisting via `config::save`.

### Off-loop executor (lines 378-458)

To keep the loop responsive, fix execution is serialised on a dedicated worker:
- `ExecJob` (378-388): `action`, `decision_id`, `baseline` (`SystemState`), `label` (`format!("{action:?}")`, the dedupe key + feed label), `diagnosis`, `confidence`, `reason: Option<String>` (None = autonomous).
- `ExecOutcome` (391-400): `label`, `exec_action` (executed action's Debug form), `success`, `output`, `diagnosis`, `confidence`, `reason`.
- `spawn_executor` (407-458): consumes `ExecJob`s on an unbounded channel; each job runs `executor::execute` inside an inner `tokio::spawn` so a **panic is isolated** (JoinError → a failed `ExecutionResult` "execution task panicked", worker survives). It then `audit::log_execution`, `mark_decision_executed`, and `feedback::record`, and finally sends `ExecOutcome` back on `done_tx` (ignored if the loop is gone = shutting down).
- The loop sends jobs via `exec_tx` and folds outcomes via the `exec_done_rx` arm. Unbounded channels are used deliberately (job volume bounded by problems-per-cycle + approvals) so a send never blocks the loop.

### `actionable_fingerprint` (lines 465-525)

Returns `Some(stable_string)` only when something is worth analysing, else `None` (idle, skips the AI call). It collects sorted parts from:
- File-change log events where `severity != "INFO"` or there are error snippets (benign INFO ignored).
- Windows events with level `Error` or `Warning`.
- Each failed service; resource thresholds `CPU>90`/`MEM>90`/`DISK>90`.
- **Security gating**: firewall profiles only when `Some(false)` (off) — `None` (unknown) and `Some(true)` (on) are ignored, so a secure box stays idle. Defender faults (`realtime_off`, `sig_stale` if `signature_age_days > 3`) count **only when Defender is the active AV** (`antivirus_enabled != Some(false)`) — a third-party AV's passive Defender is treated as normal.

Identical fingerprints across cycles mean nothing changed → skipped until the 6 h heartbeat.

### `resting_status` & `should_escalate` & `restart_self`

- `resting_status` (363-374): the non-cycle status with precedence **Paused > PendingApproval > Executing > Active**. Asserted by `status_tests::resting_status_precedence`.
- `should_escalate` (271-302): pure. Returns `Some(reason)` only when advisor `enabled`, a deeper tier is configured (`escalation_model` or `escalation_effort` non-empty), `escalations_today < MAX_ESCALATIONS_PER_DAY` (=24, the provider-agnostic backstop), USD budget not spent, AND either `needs_deeper_analysis` ("the agent flagged the signals as ambiguous") or non-empty problems whose max confidence < `low_confidence_threshold` ("confidence was low"). Covered by `advisor_tests`.
- `restart_self` (228-239): spawns a detached `cmd /C "sc stop EirSvc & ping -n 4 ... & sc start EirSvc"` (LocalSystem, no UAC), surviving this process exiting — used by `UpdateSettings`.
- `spawn_update_cycle` (307-359): runs a cycle on a detached task with a nested inner `tokio::spawn` + `CYCLE_MAX = 60 min` timeout watchdog, so a panic (JoinError) or hang still produces a `CycleSummary` and releases `updater_running` — the updater can never latch "running" forever.

## Signal sources

Eir's signal layer is three independent background collectors that each maintain their own in-memory buffer, plus a per-cycle aggregation step in the decision loop that drains them into one `SignalSnapshot`. Each collector runs on its own cadence and writes to a shared, lock-guarded buffer; the decision loop reads a consistent slice of all three on each tick. All collectors live under `eir-svc/src/signals/` (`mod.rs` is just the module list — `event_log`, `file_watch`, `log_parser`, `wmi`).

### Data model (`eir-svc/src/models.rs`)

- **`SignalSnapshot`** (lines 5–12) is the per-cycle bundle handed to the AI: `timestamp`, `event_log: Vec<EventLogEntry>`, `file_changes: Vec<FileChange>`, `system_state: SystemState`, `decision_history: Vec<PastDecision>`. The first three come from the three collectors; `decision_history` is loaded separately from the audit DB (`audit::get_recent_decisions(&db, 5)`, `main.rs:1022`), not from a signal source.
- **`SystemState`** (lines 50–68): `uptime_secs`, `cpu_usage_percent`, `memory_usage_percent`, `memory_available_gb`, `disk_usage_percent`, `disk_free_gb`, `running_services_count`, `failed_services: Vec<String>`, `network_interfaces`, `network_errors` (hardcoded `0`), `disk_health` (hardcoded `"unknown"`), `windows_update_status`, and `security: SecurityPosture` (`#[serde(default)]` so old persisted snapshots without the field still deserialise).
- **`SecurityPosture`** (lines 72–76) = `FirewallStatus` + `DefenderStatus`, both `Default`. `FirewallStatus` (80–85) holds `domain/private/public: Option<bool>` (`true`=on, `None`=unreadable, deliberately not a fault). `DefenderStatus` (90–98): `realtime_enabled`, `antivirus_enabled`, `signature_age_days`, all `Option`, `None` when Defender is absent or the query fails.
- **`EventLogEntry`** (14–21): `timestamp`, `level`, `source`, `message`, `event_id`. **`FileChange`** (23–31): `path`, `kind`, `size_bytes`, `timestamp`, `log_event: Option<LogEvent>`. **`LogEvent`** (34–48): `program`, `log_path`, `severity` (FATAL/ERROR/WARN/INFO), `error_snippets` (≤5), `content_excerpt` (capped raw text so the AI can disambiguate a benign `"error"` JSON field from real corruption).

### Source 1 — Windows Event Log (`signals/event_log.rs`)

- **Collects:** Error/Warning/Information records from configured channels (default `["System", "Application"]`, `config.rs:282`) via the legacy Win32 EventLog API (`OpenEventLogW`/`ReadEventLogW` reading `SEQUENTIAL_BACKWARDS` = newest-first, line 16).
- **Cadence:** `event_log_poll_interval_secs`, default 45 (struct default `config.rs:280`; embedded sample uses 30; clamped to a 5 s floor on update, `config.rs:210`).
- **Cursor logic:** per-channel `HashMap<String,u32>` of the highest `RecordNumber` delivered (lines 149, 160–164). First poll primes from 0; subsequent polls return only records with `RecordNumber > last` (lines 90–94). Newest-first read stops at the first already-seen record.
- **Bounding:** `RING_SIZE = 20` (line 13) caps entries **per poll across all channels combined** (lines 167, 170, 127–130). On each tick the shared buffer is **replaced wholesale** with the new batch (`*guard = new_entries`, line 181) — so the event-log slice is "new events since last poll," not a rolling history.
- **Message extraction is intentionally shallow:** `message` is just `format!("EventID {}", id & 0xFFFF)` (line 120); full text would require loading provider DLLs ("sufficient for Phase 1"). `event_id` is masked to the low 16 bits (line 121). The blocking Win32 work runs in `spawn_blocking` (line 157).

### Source 2 — File / log watcher (`signals/file_watch.rs` + `signals/log_parser.rs`)

- **Discovery (`discover_watch_dirs`, lines 51–106):** scans fixed roots (`C:\Windows\Logs`, `C:\Windows\Temp`, `C:\Temp`, `C:\Logs`) plus env-var roots (`LOCALAPPDATA`, `APPDATA`, `PROGRAMDATA`, `TEMP`, `TMP`). A root/subdir (one level deep, `max_depth` 1–2) is watched only if it contains a recognised text file modified within `DISCOVERY_WINDOW_DAYS = 30` (lines 12, 110–131). Config `log_directories` extras are always included if they exist on disk, regardless of age. Runs in `spawn_blocking`; re-discovered every 20 decision cycles, new dirs pushed to the running watcher over an mpsc channel (`main.rs:977–995`, `file_watch.rs:179–190`).
- **Watching:** `notify` `RecommendedWatcher`, `RecursiveMode::Recursive`, on a dedicated OS thread (line 173). Reacts to `Create`/`Modify` only (lines 195–199). Shutdown is via dropping a `SyncSender(0)` handle (`TryRecv::Disconnected` → exit, lines 142–144, 177).
- **Per-event parse:** for each changed path it reads `size_bytes` and calls `try_parse_log` (lines 200–209). `try_parse_log` (27–42) skips empty files and files `> MAX_READ_BYTES = 65_536` (line 11), requires one of `TEXT_EXTENSIONS` (log/txt/csv/json/xml/ini/cfg/conf/err/out/trace/debug/warn/error/info, lines 14–17), reads the whole file, and runs `log_parser::parse`. A result that is INFO with no error snippets is dropped to `None` (line 37) — only "interesting" log events attach to the `FileChange`.
- **`log_parser::parse`** (`log_parser.rs:38–48): infers `program` from path shape (Program Files / ProgramData / Windows\Logs\<Subsystem> / AppData\(Local|Roaming|LocalLow), lines 64–125, falling back to parent dir name); `extract_errors` (129–171) walks lines, classifies against `ERROR_KEYWORDS`/`WARN_KEYWORDS` (lines 4–30), raises a severity ceiling (FATAL > ERROR > WARN > INFO), and collects up to 5 non-overlapping snippets (1 line before + 2 after, lines 161–167); `excerpt` caps raw content at `MAX_EXCERPT_CHARS = 2500` with a truncation marker (lines 35, 52–60).
- **Bounding:** `RING_SIZE = 50` (line 10), a true rolling ring buffer (`pop_front` when full, lines 211–214). Unlike the event log, file changes are **drained** (`drain(..)`, lines 228–233), so each `FileChange` is delivered to the AI at most once.

### Source 3 — System state / WMI (`signals/wmi.rs`)

- **Cadence:** `wmi_poll_interval_secs`, default 300 (`config.rs:281`), 30 s floor on update (`config.rs:211`). `snapshot_state` runs in `spawn_blocking`; the result **replaces** the single shared `Option<SystemState>` (lines 437, 447–449). `current()` clones it, or returns an all-zero placeholder if nothing has been collected yet (462–482).
- **What it collects** (mostly direct Win32, not WMI, despite the name):
  - Uptime: `GetTickCount64` (lines 31–33).
  - Memory: `GlobalMemoryStatusEx` → load % + available GB (83–94).
  - Disk: `GetDiskFreeSpaceExW` on `C:\` → usage % + free GB (96–115).
  - Services: `OpenSCManagerW` + `EnumServicesStatusExW` (two-pass: size then read) over active Win32 services; counts running and lists names not in `SERVICE_RUNNING` as `failed_services` (117–199).
  - Network interfaces: `GetAdaptersInfo`, marking each up/down by whether it has a non-`0.0.0.0` IPv4 (207–242).
  - Windows Update: registry read of `…\WindowsUpdate\Auto Update\Results\Install\LastSuccessTime` → `"last_install: <time>"` or `"unknown"` (244–289).
  - CPU: the **only real WMI call** — `Get-WmiObject Win32_Processor … LoadPercentage` via PowerShell (74–81).
- **`ps_capped` (the bounded probe, lines 41–70):** spawns `powershell.exe -NonInteractive -NoProfile -Command`, polls `try_wait` every 100 ms, and on deadline `kill()`s the child and returns `None`. Rationale documented inline (35–40): the snapshot loop awaits each `snapshot_state` before the next tick, so a wedged `Get-MpComputerStatus` on a degraded box would otherwise stall **every** signal; the cap bounds it. Two probes use it — CPU and Defender — both with a **15 s** timeout. Outputs are tiny (one line), so reading after exit can't deadlock the pipe.
- **GPO-aware firewall (`get_firewall` + `effective_firewall`, lines 330–362):** reads `EnableFirewall` REG_DWORD from both the local store (`…SharedAccess\Parameters\FirewallPolicy\{DomainProfile|StandardProfile|PublicProfile}`) and the GPO store (`…Policies\Microsoft\WindowsFirewall\{DomainProfile|PrivateProfile|PublicProfile}`). Note the naming mismatch: local calls private "StandardProfile", GPO calls it "PrivateProfile" (handled at lines 358–360). `effective_firewall` (330–336): policy ON → `Some(true)`; **policy OFF → `None`** (a GPO is deliberately holding it off and `netsh` can't override, so Eir treats it as "not ours to fix," preventing a futile `firewall_enable` loop on managed machines); no policy → the local value. Unreadable stays `None` so the AI never reads "couldn't read it" as "firewall off." `read_reg_dword` (294–321) checks the value **type** is `REG_DWORD` and length is 4, rejecting a 4-byte string/binary value rather than misreading it.
- **Defender parse (`get_defender` + `parse_defender_status`, lines 364–395):** one `ps_capped` call runs `Get-MpComputerStatus -ErrorAction SilentlyContinue` and formats `'{0}|{1}|{2}'` from `RealTimeProtectionEnabled|AntivirusEnabled|AntivirusSignatureAge`. `parse_defender_status` splits on `|`; each field parses independently to `Some`/`None` (bools tolerant of casing/whitespace, age as `u32`), so any garbage/empty field degrades to `None` instead of failing the snapshot. Absent Defender / timeout → empty output → all `None`.
- Unit tests in-file cover the firewall GPO matrix and Defender parsing (lines 484–529).

### Aggregation and the actionable fingerprint (`eir-svc/src/main.rs`)

- **Wiring:** all three spawn at startup (`event_log::spawn` 658, `file_watch::spawn` 673 after `discover_watch_dirs`, `wmi::spawn` 675); a 5 s settle sleep follows before status flips to "Active" (677).
- **Per cycle:** the decision loop builds the snapshot from `event_log::snapshot` (replace-semantics read), `file_watch::drain` (one-shot read), `wmi::current` (clone of latest), plus DB decision history (`main.rs:1028–1034`).
- **`actionable_fingerprint` (lines 465–525)** decides whether a cycle is even worth an AI call and dedups unchanged states. It includes: file `log_event`s where severity ≠ INFO or snippets exist (`F|path|sev|count`); Error/Warning event-log entries (`E|level|source|id`); each `failed_service` (`S|name`); CPU/MEM/DISK `>90` flags; firewall profiles that are explicitly `Some(false)` (`FW|name`) — `None`/`true` are excluded so a secure box stays idle; and Defender faults (`DEF|realtime_off`, `DEF|sig_stale` for age `>3`) **only when `antivirus_enabled != Some(false)`** (a third-party AV having taken over makes Defender's passive state normal). Parts are sorted and joined; **empty → `None` → skip the Claude call.** Identical fingerprint across cycles means nothing changed, so it's skipped (`main.rs:1071–1116`).

## AI layer & prompts

The AI layer lives in `eir-svc/src/ai/` (`mod.rs` re-exports `client` and `prompt`). It turns a `SignalSnapshot` into a structured `ClaudeDecision` (analysis + ranked problems + proposed fix actions), behind a provider abstraction that covers four backends. The monitoring loop in `eir-svc/src/main.rs` drives it, layers advisor-mode escalation on top, and records token/cost usage. By the codebase's layering convention this is an infrastructure adapter (concrete HTTP/subprocess clients) plus a pure prompt-builder; the domain types it produces (`ClaudeDecision`, `Problem`, `FixAction`) live in `eir-svc/src/models.rs`.

### Provider abstraction (`ai/client.rs`)

`AiClient` (`ai/client.rs:115`) wraps a single `reqwest::Client` (300s timeout, set for slow free OpenRouter models, `client.rs:209-212`) plus an internal `enum AiClientConfig` (`client.rs:120-141`) with one variant per provider:

- **`Anthropic`** — native `/v1/messages`, streaming SSE, `x-api-key` + `anthropic-version: 2023-06-01`. Requires `anthropic_api_key` and a non-empty model or `new()` bails (`client.rs:146-158`).
- **`OpenAiCompatible`** — a configured `base_url` (trailing slash trimmed) + bearer key (defaults to `"not-needed"`), `/chat/completions` streaming. Built for a local `claude-max-api-proxy` (`client.rs:159-170`).
- **`OpenRouter`** — OpenAI-compatible against `https://openrouter.ai/api/v1`. Key comes from config or is auto-detected from `~/.openrouter/config.json` under any `C:\Users` profile (`resolve_openrouter_key`, `client.rs:781-797`); blank model defaults to `"openrouter/free"` (`client.rs:183-188`). Adds `HTTP-Referer`/`X-Title` attribution and requests a final usage chunk.
- **`ClaudeCli`** — spawns the local `claude` binary (`--print --output-format json`, optional `--model`, optional `--effort`), no API key — it borrows a logged-in user session. `resolve_user_profile` (`client.rs:764-776`) uses the configured profile or scans `C:\Users\*\.claude\.credentials.json`; `resolve_claude_binary` (`client.rs:802-813`) tries the configured path, then `<profile>\.local\bin\claude.exe`, then PATH. When running as LocalSystem it injects `USERPROFILE`/`APPDATA`/etc. so the CLI finds the user's session (`client.rs:500-509`). `kill_on_drop(true)` reaps the ~245 MB process on timeout (`client.rs:496`, 300s wait at `client.rs:526-532`).

`AiClient::new` (`client.rs:144`) is the construction seam; it's rebuilt per update cycle from the live `ApiConfig` (`spawn_update_cycle`, `main.rs:313`).

### Analysis entry points & response parsing

`analyze` (`client.rs:217`) delegates to `analyze_with` (`client.rs:232`) with no overrides. `analyze_with` is the single dispatch point:
1. Builds the prompt via `prompt::build` (`client.rs:242`).
2. Applies a `model_override` to **every** provider, and an `effort_override` to the **Claude CLI only** (other providers have no effort dial; both overrides are trimmed and ignored if empty — `client.rs:240-241`). This is the advisor escalation lever.
3. Dispatches to the per-provider call (`client.rs:243-273`), returning raw text + `Option<CallUsage>` (Anthropic native always returns `None` usage).
4. Parses: `strip_fences` removes ```` ```json ````/`~~~` fences (`client.rs:824-842`), then `serde_json::from_str::<ClaudeDecision>`; on failure it falls back to `extract_json_object` (first `{` … last `}`, `client.rs:817-822`) to handle reasoning models that wrap JSON in prose (`client.rs:283-291`). A hard parse failure propagates an error with the raw text attached.

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

`AdvisorConfig` (`config.rs:26-50`): `enabled` (default false), `escalation_model`, `escalation_effort`, `low_confidence_threshold` (default 0.6, clamped 0.0–0.95 on update), `budget_usd_per_day` (default $0.50). Effort is normalised to `low|medium|high|xhigh|max` or empty (`config.rs:156-161`). The tier is **fixed config, never AI-chosen**.

`should_escalate` (`main.rs:271-302`) is pure and returns `Some(reason)` only when **all** hold: advisor enabled; at least one lever set (model or effort); `escalations_today < MAX_ESCALATIONS_PER_DAY` (24, `main.rs:265`); `budget_usd_per_day` not yet spent (when > 0); **and** either `needs_deeper_analysis` is true ("ambiguous") or there is at least one problem whose max confidence is below `low_confidence_threshold` ("confidence was low"). A healthy/empty result never escalates.

Escalation flow (`main.rs:1122-1179`): runs the base `analyze` first, then at most one deeper `analyze_with(..., Some(escalation_model), Some(escalation_effort))`. The attempt is counted **before** the call (`advisor_escalations_today += 1`, `main.rs:1147`) so a failing escalation can't retry every cycle and defeat the cap. On success the deeper `ClaudeDecision` **replaces** the base one. The day-counters (`advisor_spent_today`, `advisor_escalations_today`, `advisor_spend_date`) reset on a UTC date change (`main.rs:1124-1129`).

### Usage / cost accounting

Per-provider usage extraction in `client.rs`: OpenRouter/OpenAI-style read a streamed final usage chunk (prompt/completion tokens + OpenRouter `cost` USD, `client.rs:449-457`); Claude CLI parses its JSON envelope `{ result, total_cost_usd, usage:{...} }` including cache token fields (`client.rs:550-563`); Anthropic native returns no usage. Both base and escalation calls log via `audit::log_usage` into the `usage_log` table (`audit.rs:123-139`) and refresh `audit::usage_summary` (24h + 7d aggregate of calls/tokens/cost, `audit.rs:142-173`) into broadcast status. Escalation cost additionally accrues to `advisor_spent_today` (`main.rs:1160`), which is what the budget cap reads.

### Web-search path (used by the updater, not monitoring)

`AiClient::complete` (`client.rs:633-667`) is a separate entry point for the app-updater to resolve installer URLs / read failures with live web search. OpenRouter uses its `web` plugin (`call_openrouter_web`, non-streaming, `client.rs:671-725`); Claude CLI uses its built-in search; Anthropic/OpenAI-compatible have no web path and **fall back to spawning the Claude CLI** (`client.rs:659-665`). `claude_model_or_haiku` (`client.rs:732-742`) coerces any non-Claude/blank model to `haiku` since only Claude models provide web search here.

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
4. The executor worker (`spawn_executor`, `main.rs:407`) is a single serialised task draining an unbounded mpsc queue. Each job runs `executor::execute(&action)` inside an inner `tokio::spawn` so a panicking action is isolated (join error ⇒ synthetic failed `ExecutionResult`, `main.rs:417`). It then writes the execution log, marks the decision executed, records feedback, and reports an `ExecOutcome` back to the loop. **Design decision (task #6):** execution was moved off the decision loop so UI/status stays responsive regardless of action duration.

`executor::execute` (`executor/mod.rs:15`) is a single big `match` over `FixAction`. Two execution styles:
- **`blocking(...)`** (`mod.rs:130`) wraps synchronous Win32/`std::process` work in `tokio::task::spawn_blocking`, mapping a join panic to an `Err`. Used by `services`, `logs`, `tasks`, `registry`.
- Direct `.await` of an async adapter (`driver`, `software`, `boot`, `process`, `security`, and the inline PowerShell variants) via `make_result(...)` (`mod.rs:140`).

`make_result` normalises `anyhow::Result<String>` into `ExecutionResult { action: format!("{action:?}"), success, output }`. **Invariant:** the Debug format of the action (`format!("{action:?}")`) is the canonical key — it is what gets written to `execution_log.action` and what `safety::rate_limited` and the `in_flight` set match on. Any change to the enum's Debug shape silently invalidates rate-limiting and dedup.

### FixAction implementations

19 variants (`models.rs:135`), each with a guard appropriate to its blast radius:

| Action | Adapter | Mechanism | Built-in guard |
|---|---|---|---|
| `ServiceRestart/Stop/Start` | `services.rs` | Win32 SCM API (`OpenSCManagerW`/`ControlService`/`StartServiceW`), restart waits up to 30s for STOPPED then RUNNING | none in adapter (policy blocklist only) |
| `LogCleanup{path,days_old}` | `logs.rs` | `walkdir`, deletes files with ext in `log/tmp/dmp/etl/blf/regtrans-ms` older than cutoff | ext allowlist; missing dir ⇒ no-op |
| `DiskCleanup{target}` | inline PS (`mod.rs:35`) | only `temp`/`tmp`/`prefetch` mapped; else "no action" | hardcoded target switch |
| `PowerShellDiagnostic{script}` | `powershell::run_diagnostic` | arbitrary script as SYSTEM | **none** — full machine access; kept off whitelist |
| `TaskDisable/Enable{task_name}` | `tasks.rs` | `Disable/Enable-ScheduledTask` via spawned `powershell.exe` (`std::process`, no timeout) | single-quote escaping |
| `RegistryReset{key,name,data}` | `registry.rs` | `Set-ItemProperty` via `std::process` (no timeout) | **`ALLOWED_KEY_PREFIXES` allowlist** (Tcpip, Session Manager, Multimedia, HKCU\SOFTWARE\Microsoft); normalises `HKEY_*` forms |
| `NetworkDiagnostic{command}` | inline PS (`mod.rs:68`) | only `flush_dns/release_renew/reset_tcp/reset_winsock`; else early-return failure | hardcoded command switch |
| `DriverDisable{name}` | `driver.rs` | `sc.exe config … start= disabled` | **`CRITICAL_DRIVERS` blocklist** (storage/bus/net/fs/usb/wdf) |
| `DriverEnable{name}` | `driver.rs` | `sc.exe config … start= demand` | none (auto-whitelisted) |
| `SoftwareUninstall{pkg}` | `software.rs` | Get-Package → registry uninstall string / msiexec | **also hard-blocked in policy** — never runs |
| `BcdEdit{element,value}` | `boot.rs` | `bcdedit /set {current}` | **`SAFE_ELEMENTS` allowlist** + shell-metachar rejection on value |
| `ProcessKill{name}` | `process.rs` | `Stop-Process -Force` | **`PROTECTED_PROCESSES` blocklist** (lsass/winlogon/csrss/…) |
| `FileDelete{path}` | inline PS (`mod.rs:116`) | refuses directories, requires file to exist, `Remove-Item -Force` (no Recycle Bin) | dir/exists guard in script + policy path blocklist |
| `FirewallEnable{profile}` | `security.rs` | `netsh advfirewall set <profile> state on` | profile mapped via allowlist |
| `DefenderSignatureUpdate` | `security.rs` | `Update-MpSignature` | safe by nature (refresh only) |
| `DefenderRealtimeEnable` | `security.rs` | `Set-MpPreference -DisableRealtimeMonitoring $false` | approval-gated (could conflict with 3rd-party AV) |

Adapter-level guards are **defence in depth**: they enforce regardless of policy.toml, mostly via const allow/block lists plus single-quote escaping (`'` → `''`) before string interpolation into PowerShell.

### PowerShell timeout helper

`executor/powershell.rs`. `run_diagnostic(script)` calls `run_diagnostic_with_timeout` with `DEFAULT_TIMEOUT = 120s` (`powershell.rs:7`). Spawns `powershell.exe -NonInteractive -NoProfile -ExecutionPolicy Bypass -Command <script>`, stdin nulled, `kill_on_drop(true)`. Wraps `child.wait_with_output()` in `tokio::time::timeout`; on timeout returns an error and the dropped future kills the process. Non-zero exit ⇒ error carrying stdout+stderr+code. The 120s ceiling bounds the executor worker (`security.rs:36` notes even the Defender pull uses the default cap deliberately).

**Inconsistency / gap:** `registry.rs` and `tasks.rs` do **not** use this helper — they spawn `std::process::Command` directly with **no timeout** and synchronously (`registry.rs:43`, `tasks.rs:18`). Those are dispatched through `blocking(...)`, so a wedged child blocks a spawn_blocking thread, not the loop, but it is unbounded.

### Policy: the Verdict gate

`policy/mod.rs`. `ExecutionPolicy::load` parses `policy.toml`. `Verdict` (`mod.rs:36`): `AutoApprove | RequireApproval(String) | Block(String)`. `evaluate(action, confidence)` applies, in strict order (`mod.rs:49`):

1. **Action-type blocklist** (`blocklist.actions`) ⇒ `Block`. Never runs, *not even with approval*. Currently only `software_uninstall`.
2. **Target blocklist** (`blocked_reason`, `mod.rs:84`) ⇒ `Block`. Service-name blocklist (case-insensitive exact match) for service actions; path blocklist (case-insensitive **prefix** match) for `LogCleanup`, `RegistryReset`, `FileDelete`.
3. **Confidence gate** — `confidence < confidence_threshold` ⇒ `Block` (does not prompt).
4. **Whitelist** — action type not in `whitelist.actions` ⇒ `RequireApproval`.
5. Otherwise ⇒ `AutoApprove`.

Ordering is the key invariant: blocklist beats whitelist (a tested property — `blocklisted_action_is_always_blocked`, `mod.rs:154`), and confidence is checked before the whitelist so a low-confidence whitelisted action is blocked rather than auto-run. `confidence_threshold` is the only live `ExecutionConfig` field; it is overwritten at startup from `config.toml`/Settings (`main.rs:613`, `pol.execution.confidence_threshold = cfg.monitoring.confidence_threshold`). `policy.toml`'s value is a fallback.

**Auto vs approval vs blocked (current policy.toml):**
- **Auto** (whitelisted, reversible/low-risk): `service_restart/stop/start`, `log_cleanup`, `disk_cleanup`, `task_disable/enable`, `registry_reset`, `network_diagnostic`, `driver_enable`, `firewall_enable`, `defender_signature_update`.
- **Approval** (off whitelist on purpose): `powershell_diagnostic`, `driver_disable`, `bcd_edit`, `process_kill`, `file_delete`, `defender_realtime_enable`.
- **Blocked outright**: `software_uninstall` (one-way door — no reinstall path). Plus any target hitting the service/path blocklists (e.g. NTDS, WinDefend, `C:\Windows\System32`).

### Safety: rate-limiting & success rate

`safety.rs`. `rate_limited(pool, action, window_mins)` (`safety.rs:8`) counts `execution_log` rows where `action = format!("{action:?}")` AND `executed_at > cutoff` AND **`success = 1`**. Returns `count > 0`.

**Critical, deliberate behaviour: rate-limiting is success-only.** A *failed* execution does **not** count, so a failing auto-action can be retried every cycle with no backoff — only a *successful* identical action suppresses re-runs within the window. This prevents pointless re-running of an already-applied fix while still allowing retries of failures; the cost is that a persistently-failing action loops unbounded (there is no failure-based circuit breaker). The match key is the exact Debug string, so two actions differing only in a field value are distinct keys.

`success_rate(pool)` (`safety.rs:24`) = `SUM(success)/COUNT(*)` across all of `execution_log`, returning `1.0` when empty. Used only for logging/warning in the loop (`main.rs:1195`, warns under 85%) — it does **not** feed back into any verdict.

### Explanations

`explain.rs`. `ActionExplanation { summary, target, reversible }` (`explain.rs:13`). `explain(action)` (`explain.rs:23`) returns a hand-written, deterministic description per variant — derived only from the action type and its fields, **never from the AI** — so the user can trust it when approving. The AI's own `side_effects`/`undo_instructions` are shown alongside as supporting detail (`main.rs:1308`). Notable: `software_uninstall`'s summary states it is policy-blocked and won't run; `powershell_diagnostic` uses `with_target_first_line` (`explain.rs:171`) to put a 60-char script snippet in `target`.

`target_details(action)` (`explain.rs:227`) gathers factual on-disk detail — for `FileDelete` it runs `file_facts` (size, last-modified+age, read-only flag, and a `classify_file` risk heuristic distinguishing regenerable cache vs personal-folder data vs config); for `PowerShellDiagnostic` it returns the full script. Does file I/O, so it is called only on the approval path, off the hot loop.

### Current gaps / dead code

- `max_retries_per_issue` and `auto_approve_on_success_rate` (`ExecutionConfig`, `mod.rs:17/19`) are **dead** — deserialized and referenced only inside `policy/mod.rs` tests; no production code reads them (confirmed by crate-wide grep). The `#[allow(dead_code)]` comment claims "used in Phase 4" but they are not. There is consequently **no retry cap** and **no success-rate-driven auto-approval promotion**.
- No failure-based backoff/circuit breaker (see success-only rate-limiting above).
- `registry.rs` / `tasks.rs` bypass the timeout helper (no execution timeout).
- `RegistryReset.reversible = false` because the prior value is never snapshotted (`explain.rs:92`); there is no generic undo mechanism for any action.

## Autonomous app updater

The updater is an AI-driven, self-healing, fully-unattended app updater that lives entirely in the LocalSystem `eir-svc` (so package managers and installers run with no UAC prompt). It is internally layered, dependencies pointing inward (per `eir-svc/src/updater/mod.rs:1`):

- a pure **domain / validation core** (`domain.rs`, `plan.rs`, `version.rs`, `names.rs`) with no I/O — the "AI proposes, Rust disposes" layer every AI proposal must pass;
- an **application orchestrator** (`orchestrator.rs`, `check.rs`, `diagnose.rs`) — the check → attempt → diagnose → retry loop;
- per-method **infrastructure adapters** (`methods/winget.rs`, `choco.rs`, `scoop.rs`, `msstore.rs`, `native.rs`, plus `download.rs`, `verify.rs`, `proc.rs`, `history.rs`).

### The cycle

One full cycle is `run_cycle` (`orchestrator.rs:303`), driven from `main.rs:307` (`spawn_update_cycle`) on a detached task with a 60-minute backstop watchdog and a `cycle_id` = `Utc::now().timestamp()` that groups the run's rows in the audit DB. The cycle:

1. **Determine available methods** — `available_methods` (`orchestrator.rs:207`): a method is usable only if enabled in config AND present on the machine. winget via `where winget`; choco via its ProgramData path (bootstrapped through the official install.ps1 if `bootstrap_managers` and missing — `detect.rs:61`); scoop only if a logged-in user already has it (never installed as SYSTEM); msstore reuses winget; `Native` is offered only when `native_enabled` and an AI client exists.
2. **Collect candidates** — `check::collect` (`check.rs:97`). Each enabled manager lists its updates (`winget upgrade`, `choco outdated -r`, `scoop status`, `winget upgrade --source msstore`). Results are de-duplicated by app identity via `push_candidate` (`check.rs:66`): earlier (more-preferred) managers win, the id is `clean_app_name(name).to_lowercase()`, and `should_skip` / the seen-set drop ignored/duplicate apps. When a primary manager handles the app, the native installer is appended as a self-healing fallback method (unless the primary already is native). Then an **AI web-search pass** over apps no manager covers (`check_unmanaged`, `check.rs:217`) produces native-only candidates.
3. **Heal each candidate** (bounded by `max_apps_per_run` and the per-run `budget_usd_per_run`) — `heal` (`orchestrator.rs:134`).
4. **Record** every attempt to the `update_attempts` table under `cycle_id` (`history::record_attempts`).

`CheckResult`/`CycleSummary` carry candidates, per-cycle AI cost, and human notes (truncation, AI-check failures, budget stop). `app_rows` (`orchestrator.rs:248`) flattens each candidate's attempts into one UI row, the winning attempt (first success, else last) deciding state: `verified` / `installed` / `failed` / `skipped`.

### Per-app multi-method self-heal

`heal` builds the candidate's method order (intersection of `candidate.methods` and `available`, preserving preference order), then loops: dispatch a method, classify the outcome, and on a **non-terminal** failure ask `decide_next` for the next step, repeating until success, a terminal integrity failure, methods exhausted, or `max_attempts_per_app` reached.

- `dispatch` (`orchestrator.rs:52`) applies any allow-listed remedy (`KillProcess` → sanitised `taskkill /IM <name> /F`, or `Force`) then calls the method adapter.
- `decide_next` (`orchestrator.rs:111`): when an AI client is configured it calls `diagnose::diagnose` (the diagnostician), otherwise the deterministic ladder `next_method`.
- **The AI is bounded twice**: a paid `Native` attempt never *starts* once the app's AI spend reaches `budget_remaining` (`orchestrator.rs:159`), and AI diagnosis is paid for only while under budget — otherwise the free deterministic next step is taken (`orchestrator.rs:180`). `app_spent` tracks spend within a single app's heal so the per-run budget is a true ceiling.
- **Reboot is never taken unattended**: a `RetryAfterReboot` remedy ends the heal (defer) rather than rebooting (`orchestrator.rs:195`).

**Rust always has the final say.** The AI diagnostician (`diagnose.rs`) is shown the *real* captured error, the failure category (classified by Rust, not the AI), and the tried/available methods, and proposes ONE `ProposedStep`. `validate_next_step` (`domain.rs:238`) disposes: an integrity-terminal failure always gives up; a `Switch` target must be available and untried; a `Retry` remedy must fit the method and failure (`remedy_ok`, `domain.rs:269`) — e.g. `Force` only on winget/choco, `ClearManagerLock` only on choco/scoop, `KillProcess` only when the name appears as a whole token of the error text; anything invalid falls back to `deterministic_next`. A malformed AI reply collapses to GiveUp, which the validator then turns into the deterministic step, so a bad reply never strands an app.

### Method order and adapters

Default preference order is `winget, choco, scoop, msstore` (`config.rs:114`); `native` is gated separately by `native_enabled` and appended as a per-candidate fallback. Each adapter returns a structured `AttemptOutcome`:

- **winget** (`winget.rs`): runs directly as SYSTEM, captures and cleans winget's output (`clean_winget_output`, ported verbatim with tests for spinner/OEM-mojibake/byte-counter stripping), auto-retries once with `--force` for the portable-modified case, verifies by id.
- **choco** (`choco.rs`): `choco outdated -r` (pipe-delimited, pinned packages skipped), `choco upgrade <id> -y`, success codes 0/3010/1641, cross-checks the new version via winget's ARP read.
- **scoop** (`scoop.rs`): user-scoped — runs the user's `scoop.cmd` shim with `USERPROFILE`/`HOME` pointed at their profile; best-effort, exit-code-only verification (scoop apps don't register in ARP).
- **msstore** (`msstore.rs`): `winget ... --source msstore`; per-user, may need the user's Store entitlement.
- **native** (`native.rs`): the AI-found installer path (below).

### proc timeouts

All external commands go through `proc::run_capped[_cmd]` (`proc.rs`), which applies `CREATE_NO_WINDOW` and a hard timeout with `kill_on_drop` so a hung child (a known winget-as-SYSTEM failure mode) can never wedge the cycle and latch `updater_running` forever. On overrun it returns the sentinel `TIMED_OUT = -4` with "timed out" text (classified as transient/non-terminal). Timeout constants: `PROBE = 30s` (presence probes), `LIST = 150s` (update listing / source refresh), `INSTALL = 600s` (download + run installer), `VERIFY = 60s` (read installed version back, signature read, exe ProductVersion read).

### Native AI-found installer + signature policy

For an app no manager can update, `update_native` (`native.rs:122`): asks the model for the OFFICIAL direct installer (`install_plan_prompt`), validates the plan, downloads + hashes + signature-gates it, runs it as SYSTEM, and verifies the version moved. Nothing the AI returns reaches the shell unchecked:

- **`validate_plan`** (`plan.rs:222`, pure, unit-tested): https-only; no credentials/non-default-port/raw-IP/punycode host; host must be a TRUSTED multi-tenant release host (`github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com` — deliberately NOT `*.github.io` or `raw/gist.githubusercontent.com`, which serve arbitrary user files) OR the app's own vendor domain by **exact brand-label equality** (`host_matches_name`, rejecting lookalikes like `obsidian-download.com`, `notionx.io`, `krita.evil.com`); URL must end `.exe`/`.msi`; silent args allow-listed by installer kind via `sanitise_args` (shell metacharacters dropped); optional 64-hex SHA-256. An `.exe` with no known silent switch is refused (`plan_runnable`) → manual fallback; an MSI defaults to `/qn /norestart`.
- **`download_and_check`** (`download.rs:345`): streams to a SYSTEM/Administrators-only staging dir under `%ProgramData%\Eir\staging` (ACL applied **fail-closed** — no install if lockdown fails; `ensure_root`). The full URL gate is re-applied on every redirect hop and the final URL; HTML bodies and over-cap sizes are rejected (header and streamed-byte counter); a vendor SHA-256, if given, must match (terminal `HashMismatch`).
- **Signature gate** (`signature_gate`, `download.rs:167`) is a HARD gate decided in Rust before launch, per `SignaturePolicy` (`config.rs:14`): `RequireValid` (default — any trusted valid Authenticode signature), `RequirePublisherMatch` (valid AND signer CN equals the expected publisher — note: `expected_publisher` is currently AI-sourced, so this is a tripwire, not a true vendor pin), `AllowUnsigned` (explicit opt-in). A rejection message starts "signature rejected" so it classifies as terminal `SignatureRejected`. A timed-out signature read yields non-"Valid" text → fails closed.
- **`run_installer`** (`native.rs:243`): re-hashes the staged file immediately before launch (TOCTOU; sentinel `-4` = changed → reported as tampering/`HashMismatch`), runs the exe directly or via `msiexec /i`, with a 10-minute watchdog. Success = exit 0 or 3010 (reboot). Verification is by name (winget ARP read) with an exe-ProductVersion fallback, softened to `Unverified` on an exe-fallback mismatch since a 4-part FILEVERSION can trail the marketing version.

### Self-updater skip (should_skip / base_id / SELF_UPDATING) and the ignore list

Two distinct skip mechanisms:

1. **`SELF_UPDATING` / `should_skip` / `base_id`** (`check.rs:35`–`60`) — the just-added skip for apps that update themselves and reliably fight package managers. `SELF_UPDATING = &["discord"]` (Discord's Squirrel installer hangs `choco upgrade` for the full INSTALL timeout, then choco's stale version DB makes it retry every cycle). `base_id` strips a Chocolatey package suffix (`.install`/`.portable`/`.app`/`.commandline`) so `discord.install` and `discord` share one identity. **`should_skip` unifies the self-updater set and the user ignore list against the same base id**: an id is skipped if its base is in `SELF_UPDATING`, OR if the user's `ignored` list matches the exact id or its base (case-insensitive) — so ignoring `discord` also covers the `discord.install` choco package, and vice versa. `should_skip` is enforced at three points: when pushing manager candidates (`push_candidate`, `check.rs:78`), when filtering the unmanaged set before the AI check (`check.rs:232`), and on the AI's returned native candidates (`native_candidates_from`, `check.rs:310`).
2. **Eir-self skip** — the `is_noise` SKIP list in `winget_parse.rs:130` excludes `"eir"` (alongside drivers/runtimes/redistributables/OS components) from the unmanaged AI check, so the app updater never tries to update Eir itself; Eir's own self-update is handled by the separate Tauri updater plugin, not this engine.

Native candidate identity is anchored to the machine (`native_candidates_from`, `check.rs:291`): an AI-reported update whose name doesn't resolve to an actually-installed app (`match_installed` against the real `winget list` set) is dropped, and only strictly-newer versions (`version::is_newer`, with the marketing-truncation guard) survive — preventing a poisoned AI from naming a fabricated app to pick an arbitrary vendor domain.

### update_attempts history

`history::record_attempts` (`history.rs:17`) inserts one row per attempt into the `update_attempts` table (migration 0007) under the run's `cycle_id`: app id/name, from/to version, method, success, category (stable snake_case token, NULL on success), exit code, signature, sha256, detail, AI cost, timestamp. `recent` feeds the UI history view (newest first); `clear` backs the UI's "Clear" on the App Updates card. This is the audit trail for unattended installs.

## Persistence, audit DB & the existing feedback loop

Eir's persistence layer is a single SQLite database, opened once at service start and shared as an `sqlx::SqlitePool` passed by reference into every read/write helper. There is no ORM and no repository abstraction: each table has hand-written `INSERT`/`SELECT` queries with positional binds, grouped by concern into three modules — `eir-svc/src/audit.rs` (decisions, executions, usage, approvals), `eir-svc/src/feedback/mod.rs` (before/after outcome scoring), and `eir-svc/src/updater/history.rs` (autonomous-updater attempt log). The schema lives in `migrations/*.sql` and is applied at boot via the embedded `sqlx::migrate!("../migrations")`.

This is the *only* durable substrate Eir has to learn from. Importantly, today the loop's "learning" is entirely prompt-based: a human-readable feedback summary plus the last 5 decisions are injected into the AI prompt each cycle. There is no model fine-tuning, no policy auto-adjustment, and several rich tables (`system_state_history`, the full `signal_snapshot`, per-attempt updater costs) are written but never read back into any decision.

### Database bootstrap

`audit::init_db(path)` (`eir-svc/src/audit.rs:12`) builds `SqliteConnectOptions` from `sqlite:{path}?mode=rwc` with `create_if_missing(true)`, connects a pool, and runs all migrations. The DB path comes from `config.persistence.audit_db` (a plain string field, `eir-svc/src/config.rs:163`); the sample default is `./eir.db` (`config.rs:264`). Migrations are versioned `0001`–`0007` and applied in order on every start (idempotent — all use `CREATE TABLE IF NOT EXISTS`).

### Full schema (every table)

**`decisions`** (`migrations/0001_initial.sql:1`) — one row per AI analysis that produced a result.
- `id INTEGER PK AUTOINCREMENT`
- `timestamp TEXT` (RFC3339 UTC)
- `signal_snapshot TEXT` — full `SignalSnapshot` serialized as JSON (event log, file changes, system state, decision history)
- `claude_response TEXT` — full `ClaudeDecision` JSON (analysis, problems[], needs_deeper_analysis)
- `confidence REAL` — max problem confidence across the decision
- `executed INTEGER DEFAULT 0` — flipped to 1 once any action from this decision runs
- `execution_output TEXT` — **declared but never written or read** (executions go to `execution_log` instead; dead column)

Written by `audit::log_decision` (`audit.rs:21`); `executed` flipped by `audit::mark_decision_executed` (`audit.rs:68`); read by `audit::get_recent_decisions` (`audit.rs:76`).

**`system_state_history`** (`0001_initial.sql:11`) — a metrics time-series row written alongside every decision.
- `id`, `timestamp TEXT`, `cpu_usage REAL`, `memory_usage REAL`, `disk_usage REAL`, `failed_services_count INTEGER`, `snapshot TEXT` (full `SystemState` JSON).
- Written inside `audit::log_decision` (`audit.rs:51`). **Never read anywhere** — pure write-only history, currently unused as a learning signal.

**`execution_log`** (`migrations/0002_execution_log.sql`) — one row per fix-action execution.
- `id`, `decision_id INTEGER NOT NULL → decisions(id)`, `action TEXT` (the executed action string), `success INTEGER`, `output TEXT` (stdout/stderr or error text), `executed_at TEXT`.
- Written by `audit::log_execution` (`audit.rs:261`), called from the executor worker (`main.rs:426`). Read by `safety::success_rate` (aggregate) and joined in `feedback::recent_summary` for failure-reason text.

**`execution_feedback`** (`migrations/0003_feedback.sql`) — the before/after outcome record; the heart of the feedback loop.
- `id`, `execution_log_id INTEGER → execution_log(id)`, `action TEXT`, `succeeded INTEGER`
- `cpu_before REAL`, `memory_before REAL`, `failed_services_before INTEGER` — captured at execution time
- `cpu_after REAL`, `memory_after REAL`, `failed_services_after INTEGER` — filled on the **next** decision cycle (NULL until then)
- `improvement_score REAL` — computed when after-state is filled
- `recorded_at TEXT`
- Written by `feedback::record` (`feedback/mod.rs:7`, right after `log_execution` at `main.rs:431`); after-states + score filled by `feedback::update_after_states` (`feedback/mod.rs:35`); read by `feedback::recent_summary` (`feedback/mod.rs:112`).

**`usage_log`** (`migrations/0005_usage.sql`) — per-call AI token/cost accounting (populated when the provider returns usage, primarily `claude_cli`).
- `id`, `timestamp TEXT`, `input_tokens`, `output_tokens`, `cache_creation`, `cache_read` (all INTEGER), `cost_usd REAL`.
- Written by `audit::log_usage` (`audit.rs:123`); aggregated by `audit::usage_summary` (`audit.rs:142`) over 24h / 7d windows into `UsageSummary { calls, tokens, cost }` shown in the UI.

**`pending_approvals`** (`migrations/0006_pending_approvals.sql`) — actions awaiting the user's decision, persisted so an approval survives idle cycles and service restarts (replaces an old blocking timeout flow). The **row id is the approval id surfaced to the UI**.
- `id`, `created_at TEXT`, `decision_id INTEGER → decisions(id)`
- `action_json TEXT` — serialized `FixAction`, executed verbatim on approval
- `info_json TEXT` — serialized `ApprovalInfo` for the UI (its `id` field overwritten from the row id on load)
- `baseline_json TEXT` — `SystemState` at proposal time, the "before" baseline for feedback once executed
- Written by `audit::insert_pending_approval` (`audit.rs:180`, from `main.rs:1317`); loaded at startup by `audit::load_pending_approvals` (`audit.rs:211`); removed by `audit::delete_pending_approval` (`audit.rs:253`, on approve/reject at `main.rs:840`).

**`update_attempts`** (`migrations/0007_update_history.sql`) — append-only log of every autonomous-updater attempt, grouped by `cycle_id`.
- `id`, `cycle_id INTEGER` (groups one run), `app_id TEXT` (version-stripped identity), `name TEXT`, `from_version TEXT`, `to_version TEXT`, `method TEXT` (winget|choco|scoop|msstore|native), `success INTEGER`, `category TEXT` (failure `ErrorCategory` token, NULL on success), `exit_code INTEGER`, `signature TEXT` (Authenticode result, native), `sha256 TEXT` (installer hash, native), `detail TEXT` (cleaned reason), `cost_usd REAL` (AI spend attributable to the attempt), `created_at TEXT`.
- Written by `updater::history::record_attempts` (`updater/history.rs:17`); read by `updater::history::recent` (limit 50, `history.rs:61`, → `eir_proto::UpdateAttemptRow{name, method, success, detail, at}`); wiped by `updater::history::clear` (`history.rs:53`, UI "Clear" button). Note: `from_version`, `category`, `exit_code`, `signature`, `sha256`, `cost_usd` are **stored but not surfaced** by `recent` — available for analysis but unused.

**Indexes** (`migrations/0004_indexes.sql`, `0005`, `0007`): `execution_log(action)`, `execution_log(executed_at)`, `execution_feedback(cpu_after)`, `usage_log(timestamp)`, `update_attempts(app_id, created_at)`, `update_attempts(cycle_id)`.

### How the feedback loop works today (control + data flow)

1. **Decision logged.** Each cycle, after the AI returns, `audit::log_decision` writes the `decisions` row (returning `decision_id`) plus a `system_state_history` row (`main.rs:1181`).
2. **Approval or execution routed per problem.** For each problem, the proposed fix is policy-gated (`main.rs:1205`+). `RequireApproval` → `insert_pending_approval` persists it (`main.rs:1317`); auto-approve → handed to the executor worker.
3. **Execution + baseline capture.** The executor worker (`spawn_executor`, `main.rs:407`) runs the action panic-isolated, then writes `execution_log` (`log_execution`), marks the decision executed (`mark_decision_executed`), and writes the `execution_feedback` "before" row via `feedback::record` using the baseline `SystemState` captured at proposal/execution time (`main.rs:426`–`441`).
4. **After-state measurement.** On the *next* cycle, once fresh signals are collected, `feedback::update_after_states` (`main.rs:1060`) finds up to 50 rows with `cpu_after IS NULL`, fills `cpu_after`/`memory_after`/`failed_services_after`, and computes `improvement_score`.
5. **Scoring.** `improvement_score` (`feedback/mod.rs:75`) = `cpu_delta*0.3 + mem_delta*0.3 + fs_delta*10.0`, where each delta is `before - after` (a drop in CPU/memory/failed-service count is positive). Fixed weights; heavily favours reducing failed services. Positive = improved, negative = degraded.
6. **Feedback into the AI.** `feedback::recent_summary(db, 10)` (`main.rs:1064`) builds a human-readable bullet list of the last 10 outcomes — `"- {ts}: {action} -> SUCCESS, improved (+N)"` or `FAILURE ... [reason: …]`. For failures it `LEFT JOIN`s `execution_log.output` and condenses it to one whitespace-normalised line capped at 200 chars (`condense_reason`, `feedback/mod.rs:94`) so the model can avoid re-proposing a fix that already failed. This string is passed to `ai.analyze(snapshot, history, Some(&feedback_summary))` (`main.rs:1092`).
7. **Decision history into the AI.** Separately, `audit::get_recent_decisions(db, 5)` (`main.rs:1022`) reconstructs the last 5 decisions as `PastDecision { timestamp, diagnosis, confidence, fix_proposed }` (one entry per problem) and is embedded in the `SignalSnapshot.decision_history` *and* passed as `history` to `analyze`.
8. **Success-rate telemetry.** `safety::success_rate` (`safety.rs:24`) = `SUM(success)/COUNT(*)` over all of `execution_log`; logged each cycle (`main.rs:1195`) and warns below 85%. It is **observability only** today — it does not gate or auto-adjust anything. (`policy.toml`'s `auto_approve_on_success_rate = 0.95` and `max_retries_per_issue` are parsed but marked dead-code "Phase 4", `policy/mod.rs:14`.)

So the entire closed loop is: execute → record before → next-cycle record after + score → summarise into prompt. The AI is the only consumer of the learning; everything else is for the UI or unused.

### Config load/save and `to_ui_settings`

`config.rs` defines `Config { api, monitoring, persistence, updater (#[serde(default)]), advisor (#[serde(default)]) }`. The two `#[serde(default)]` sections let an older `config.toml` (written before those features) still parse — covered by round-trip tests (`config.rs:267`–331).

- **`load(path)`** (`config.rs:242`) resolves the path relative to the **executable directory** (`resolve`, `config.rs:231` — absolute paths pass through, since LocalSystem's cwd is unreliable), reads the file, and `toml::from_str`s it with context errors.
- **`save(config, path)`** (`config.rs:221`) serialises with `toml::to_string_pretty` and writes to the resolved path.
- **`to_ui_settings`** (`config.rs:170`) projects `Config` into `eir_proto::UiSettings` for the tray app. Crucially it **never sends secrets** — API keys are reduced to `*_key_set: bool` flags via the local `set` closure (true iff present and non-empty). It surfaces provider, model, update-check model, effort, base URL, the three poll/decision intervals, channels/dirs, and `confidence_threshold`.
- **`apply_update(SettingsUpdate)`** (`config.rs:191`) applies a UI edit: blank/None secret fields **keep the stored value** (the `keep` closure), so the UI never needs to re-send keys it can't read back. Intervals are floored (`decision ≥10`, event-log `≥5`, wmi `≥30`); `confidence_threshold` is clamped to `0.50–0.95`; `effort` is `normalize_effort`'d to one of low|medium|high|xhigh|max or empty. `ApiProvider::parse` accepts legacy aliases (`open_router`, `open_ai_compatible`).
- **Settings save triggers a self-restart.** In the loop (`main.rs:802`), an `UpdateSettings` message is validated by constructing an `AiClient` first (rejecting e.g. a keyless provider, reloading the prior config on failure so the service isn't bricked), then `config::save` + `restart_self()`. Config is reloaded from disk on next boot; it is not hot-reloaded.

`AdvisorConfig` (`config.rs:26`) has its own `to_view`/`apply_view` with the same clamping discipline (threshold clamped `0.0–0.95`, budget `≥0`). It is config-only and not stored in the DB.

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
  per-day/USD budget) may attach a plain-English explanation or a *strictly narrower*
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

### Open questions (to resolve at implementation)

- `FixIneffective` join key: `execution_feedback.action` stores the Debug form of the
  action, not `action_type_name` — normalise before grouping.
- `RejectedSignal` needs the fingerprint at rejection time: store it on the
  pending-approval/decision at proposal time, or re-derive from `decisions.signal_snapshot`.
- Make thresholds/windows/half-lives config (`policy.toml`/`UpdaterConfig` with
  `#[serde(default)]`) rather than consts, per the project's config-over-magic-numbers bias.
- `improvement_score` is noisy (cpu/mem weighted 0.3 vs failed_services 10.0) — prefer the
  failed-services component for `FixIneffective`.
- Where each detector runs (updater cycle vs decision loop) to avoid redundant scans.


---

## Known limitations & backlog

Current gaps surfaced while mapping each subsystem (the self-improvement plan above addresses several).

**Workspace, build & delivery pipeline**

- I did not open ui/index.html or ui/main.js contents, eir-ui/src/main.rs, or eir-ui/gen/; the frontend/wiring internals (how main.js uses withGlobalTauri, how the composition root injects adapters) are out of scope for this build/delivery map and unverified here.
- I confirmed the staging COPY logic and gitignore status statically but did not execute a full `cargo tauri build` to observe the bundle being produced, nor the release workflow end-to-end (compile/static-verified, not run-verified). Whether the signing secrets are actually configured in the GitHub repo is unverifiable from the source tree.
- The two build.rs files: the root build.rs reads `tauri_build::build()` (3 lines); eir-ui/build.rs is 42 bytes and was not separately opened — its exact content is assumed equivalent but unconfirmed.
- policy.toml and config.toml.example are referenced as bundle resources but I did not read them; their schema/contents are out of scope here.
- The release process is described as manual version-bump + tag push inferred from CLAUDE.md conventions and git history ([release] markers); there is no scripted bump tool in the read files, so the exact human steps are convention, not enforced by code.
- No automated check verifies the four version locations stay in sync (Cargo.toml x3 + tauri.conf.json); a drift would not be caught by the CI gate as currently written.

**Pipe protocol & tray UI**

- Single-client only: the listener serves one UI connection at a time and aborts/re-accepts on disconnect; concurrent UIs are not supported (pipe_server.rs:126-184).
- No authentication beyond the ACL: any Authenticated User at Medium+ integrity on the machine can open the pipe and send commands (approve fixes, change settings, trigger updates). The trust boundary is the local interactive logon, not the specific user (pipe_server.rs:33).
- Command delivery is fire-and-forget with no per-command ack/result: a Tauri command returning Ok only means the UiMsg was queued to the writer, not that the service applied it; the UI infers success only from the next polled snapshot (main.rs:34-112).
- Optimistic UI can drift from truth: Approve/Reject disable buttons and 'Ignore' dims the row immediately, but if the service rejects or never applies the command there is no negative feedback beyond the row reappearing on the next 2s poll (main.js:271-280,472-478).
- Status freshness is bounded by the 2s UI poll plus the service's broadcast cadence; the UI cannot request an on-demand refresh from the service (it only re-reads the local cache) (main.js:636-637).
- mpsc command channels are bounded (service 8, UI 16); a stalled writer could in principle apply backpressure, though commands are low-volume (pipe_server.rs:60, main.rs:244).
- Byte-mode pipe relies entirely on the newline delimiter for framing; a payload containing a literal newline would break framing, but serde_json::to_string emits single-line JSON so this is not currently reachable.
- tauri.conf.json (CSP, updater public key, frontendDist/beforeBuildCommand) was not read in this pass; the main.js comment that inline handlers are CSP-blocked (main.js:452-453) was not independently verified against the config.

**Service decision loop, state & off-loop executor**

- The dedupe key is format!("{action:?}") (Debug of FixAction). Correctness of dedupe and the activity-feed label depends entirely on the Debug impl being stable and uniquely identifying actions; two semantically distinct actions with the same Debug string would collide, and a Debug change would break matching against persisted pending.info.action strings.
- exec_done_rx feedback record uses result.action (the executed Debug form) while in_flight/pending were keyed on the pre-execution label; the code assumes these match — not verified here whether executor::execute can alter the action's Debug form.
- build_status clones the full recent_problems/recent_executions deques and pending vec on every broadcast; at the 20-item caps this is cheap, but every state mutation triggers a full snapshot clone+broadcast.
- Standalone (non-SCM) mode runs the same eir_main but with a Ctrl-C shutdown; behavior parity with service mode (e.g. restart_self issuing sc commands) is not exercised in dev unless the service is actually registered.
- I read only eir-svc/src/main.rs as instructed; the contracts it depends on (policy::evaluate verdict semantics, executor::execute, ai::analyze/analyze_with usage reporting, audit/* DB schema, explain::*, signals::*) are referenced but not verified in this pass.
- The pipe is stated to be writable by any authenticated user; RunUpdatesNow and Approve are gated, but this section did not audit whether every other UI command (e.g. UpdateSettings -> restart_self, SetAdvisorSettings) is adequately privilege-checked against a Medium-integrity caller.

**Signal sources**

- Event log captures only Error/Warning/Information levels and no message body (just 'EventID <n>'); Critical/Verbose levels and event descriptions are not collected. event_id is truncated to 16 bits.
- Event-log buffer is replaced each poll, so events arriving faster than RING_SIZE=20 per channel-batch (or older than the last poll) can be missed; there is no durable history of events beyond the most recent poll.
- CPU usage depends on a PowerShell WMI call; if powershell.exe is unavailable, slow, or wedged past 15s, cpu_usage_percent silently becomes 0.0 (indistinguishable from an idle CPU). Same single-point dependency for Defender.
- network_errors is hardcoded 0 and disk_health is hardcoded 'unknown' — neither is actually measured; disk metrics only cover the C: volume.
- File discovery only scans fixed roots + a small env-var root set, one subdirectory level deep, with a 30-day recency window; logs outside those roots/age or deeper than one level are not auto-watched (only explicit config log_directories extras bypass this).
- File parsing skips files >64KB entirely, so errors near the tail of a large rolling log are invisible unless captured by the keyword snippets within the first read — but try_parse_log reads the whole file only when <=64KB, so large active logs are not parsed at all.
- Keyword-based severity classification (log_parser) is substring matching with no structured-log awareness, so it can over- or under-flag (the content_excerpt is provided to let the AI correct this downstream).
- ps_capped uses sleep-poll at 100ms granularity rather than async wait; acceptable but each probe blocks a spawn_blocking thread for up to 15s.
- Several Win32 calls (event log record parsing, service enum, adapter enum) use unsafe pointer walking; correctness depends on OS-supplied buffer layout and is not covered by tests (only firewall and Defender parsing are unit-tested).

**AI layer & prompts**

- Anthropic native and OpenAI-compatible providers return no usage/cost (CallUsage None), so for those the USD budget cannot bound escalation spend — only the MAX_ESCALATIONS_PER_DAY=24 count cap applies (documented in code at main.rs:261-265).
- FixAction still contains a `SoftwareUninstall` variant (models.rs:181) even though the prompt explicitly forbids uninstalling software ('there is no reinstall path'); the variant is reachable if a model emits it, gated only downstream by policy/parse, not by the type.
- JSON parsing relies on first-{ to last-} extraction as a fallback (extract_json_object); a model that emits multiple JSON objects or stray braces in prose could be mis-parsed, and a hard parse failure aborts the whole cycle (main.rs:1105-1111).
- The prompt is a single monolithic format! string mixing schema, action catalogue, and ~110 lines of guardrail prose (prompt.rs:28-186); there is no prompt-level test verifying the action examples stay in sync with the FixAction enum tags.
- No prompt caching is used on the Anthropic native path (cache_creation/cache_read are only populated from the Claude CLI envelope), so the large static guardrail prose is re-sent uncached every cycle on that provider.
- needs_deeper_analysis and the confidence threshold are the only escalation triggers; there is no escalation on outright AI-call failure or on a parse failure — those just error the cycle.
- Advisor day-counters (spent_today / escalations_today) are in-memory process state reset only on a UTC date flip; they are not persisted, so a service restart resets the daily budget/count ceiling.
- The 'last 5 decision history' (prompt) and 'feedback last 10' windows are hard-coded at the call sites (main.rs:1023, feedback recent_summary(db,10)) rather than configurable.

**Executor, policy, safety & explanations**

- max_retries_per_issue and auto_approve_on_success_rate (policy/mod.rs:17,19) are dead code — deserialized and used only in policy tests; no production code reads them. The #[allow(dead_code)] comment's 'used in Phase 4' claim is inaccurate.
- No retry cap and no failure-based circuit breaker: combined with success-only rate-limiting, a persistently failing auto-action loops every cycle indefinitely.
- No success-rate-driven auto-approval promotion is implemented; success_rate() only feeds a log/warning at main.rs:1195, never a verdict.
- registry.rs and tasks.rs spawn powershell.exe via std::process::Command with no timeout, bypassing the powershell.rs timeout helper (bounded only by running on a spawn_blocking thread).
- No generic undo: RegistryReset is marked reversible=false because the prior value is never snapshotted (explain.rs:92).
- Rate-limit and dedupe correctness depend on the action's Debug format being stable; any change to FixAction's derived Debug silently breaks both.
- I did not exercise any action in a running service — findings are read-from-source only (compile/run not verified here).

**Autonomous app updater**

- RequirePublisherMatch's expected_publisher is AI-sourced, so it is a tripwire (valid signature whose CN equals the claimed publisher), not a true vendor pin; the code itself notes a trusted per-app publisher map would harden it.
- Scoop and msstore are best-effort under SYSTEM: scoop can't fully reproduce the user's PATH (git etc.) and verifies by exit code only (no ARP); msstore apps are per-user and may require the user's Store entitlement, applicable only while that user is signed in.
- clean_app_name folds same-base products that differ only by a dotted major (e.g. Python 3.11 vs 3.12) to one identity key — a documented, accepted limitation tolerated because heavyweight runtimes are filtered as noise upstream.
- is_newer cannot distinguish a legitimate bare-major bump from a driver cross-product false positive (e.g. 551.86.0.0 -> 552); the marketing-truncation guard deliberately leaves those alone, and the only recourse for the residual case is the per-app Ignore list.
- SELF_UPDATING is a hard-coded single entry ('discord'); other genuinely self-updating apps would need code changes or a user Ignore entry rather than configuration.
- The scoop run_scoop timeout kills the cmd shim but a grandchild git/scoop process may briefly linger (noted in scoop.rs).
- AI web-search check is capped at AI_CHECK_CAP = 20 apps per cycle (check.rs:19); beyond that, only the first 20 unmanaged apps are checked (surfaced as a UI note).
- The native signature/install path is download/hash/signature compile- and unit-verified (pure gates exhaustively tested), but running an installer as SYSTEM and the live AI plan/web-search path are not exercised in the test suite — verification level is compile/unit, not live-run.

**Persistence, audit DB & the existing feedback loop**

- No model/policy learning: feedback is purely prompt injection of a text summary. Nothing in the DB auto-tunes confidence_threshold or policy; success_rate only logs/warns.
- system_state_history (full per-cycle metrics + SystemState JSON) is written every cycle but NEVER read — a rich time-series learning signal currently unused.
- decisions.execution_output column is declared but never written or read (dead column; executions live in execution_log).
- update_attempts stores from_version, category, exit_code, signature, sha256, cost_usd but updater::history::recent surfaces only name/method/success/detail/created_at; per-attempt AI cost and failure category are persisted yet unused for any learning or aggregation.
- improvement_score uses fixed hand-tuned weights and only 3 dimensions (cpu/mem/failed-services); it ignores disk and any other signal, and an action whose effect lands beyond one cycle is mis-scored.
- after-state attribution is coarse: update_after_states stamps the SAME current system state onto ALL pending feedback rows, so concurrent/overlapping fixes share one after-snapshot and cannot be individually credited.
- recent_summary/get_recent_decisions are small fixed windows (10 and 5); long-horizon patterns across the full history are not mined.
- improvement_score / feedback math have no unit tests in feedback/mod.rs (only updater::history has a round-trip test); scoring correctness is unverified.
- config has no schema versioning/migration mechanism; forward-compat relies on per-field serde(default) and would silently drop unknown keys on save (toml round-trips only known fields).
- All AI usage cost is recorded in usage_log only when the provider returns usage (chiefly claude_cli); other providers may leave cost/token data empty, skewing usage_summary.
