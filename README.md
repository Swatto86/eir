<div align="center">

<img src="icons/128x128.png" alt="Eir" width="96" height="96" />

# Eir

**An autonomous Windows system-repair agent.**

Eir watches your machine's health, diagnoses problems with an AI model, and fixes
them — asking for approval before anything risky.

</div>

---

## What it is

Eir is a background agent for Windows that continuously monitors system health —
event logs, failed services, disk pressure, memory, network errors — and uses an
AI model as its reasoning engine to work out *what's actually wrong* and *the
least-destructive way to fix it*.

It runs as a pair:

- **`EirSvc`** — a Windows service running as **LocalSystem**, so it can read
  protected logs and apply fixes without a UAC prompt. It does the monitoring,
  reasoning, and (approved) repairs.
- **Eir tray app** — a lightweight desktop UI that shows current status, recent
  problems and executions, AI usage/cost, learned machine-specific patterns, and
  app updates. It's where you approve fixes and change every setting.

The two talk over a secured local named pipe (`\\.\pipe\EirSvc`).

> The name comes from **Eir**, the Norse goddess of healing — the agent doesn't
> just *watch* the system, it *mends* it. (Pronounced "air".)

## How it works

```
            ┌──────────────────────────┐         ┌───────────────────────────┐
            │   Eir tray app (UI)      │  named  │   EirSvc (LocalSystem)    │
            │   - status / approvals   │◄──pipe──►│   - signal collection     │
            │   - settings / usage     │  (JSON) │   - AI diagnosis          │
            │   - app updates          │         │   - policy + execution    │
            └──────────────────────────┘         └─────────────┬─────────────┘
                                                                │
                                                       ┌────────▼─────────┐
                                                       │   AI provider    │
                                                       │ OpenCode / Claude│
                                                       │ / Codex / Cursor │
                                                       │   CLI (as user)  │
                                                       └──────────────────┘
```

Each decision cycle (default every 10 minutes):

1. **Collect signals** — Windows Event Log channels, service states, CPU/memory/disk,
   network errors, security posture (firewall & Defender), watched log directories.
2. **Decide whether to think** — Eir only calls the AI when something *actionable*
   has changed (a fingerprint of the current problems), plus a periodic heartbeat so
   a healthy machine still reports in. Idle cycles are essentially free.
3. **Diagnose** — the AI returns a structured list of problems, each with a
   confidence score and a proposed root-cause fix.
4. **Gate through policy** — findings below your confidence threshold (default 80%,
   adjustable in Settings) and benign Windows noise are dropped; software uninstall
   is *never* executed; a few catastrophic actions (boot-config edits, driver
   disabling, arbitrary PowerShell) always require approval.
5. **Execute** — reversible whitelisted fixes (service restart/start/stop, log/disk
   cleanup, task enable/disable, firewall re-enable, Defender signature update) run
   automatically at or above the confidence threshold. Every registry reset and
   anything disruptive or irreversible is queued for approval in the tray UI — each
   item explains, in plain English, exactly what it will do (and, for a file delete,
   the file's real size, age, and what kind of file it is). The queue is persistent:
   it never times out and survives a service restart. If the service stops after a
   click but before recording completion, the item returns for fresh approval rather
   than replaying a possibly completed action.
6. **Learn conservatively** — Eir mines its own audit history for repeated local
   patterns, such as package-manager methods that always fail for a specific app or
   fixes that never improve a recurring issue. Learned facts can only reduce or
   reorder actions, never make Eir more aggressive, and every fact is visible in the
   UI with Pin / Disable / Forget controls.

> **Architecture & design:** see [ARCHITECTURE.md](ARCHITECTURE.md). Release
> gates and the next priorities are in [PLAN.md](PLAN.md).

## AI providers

Everything is configurable in the **Settings** panel — no file editing required.

Four CLI providers (no API keys pasted into Eir):

| Provider | Cost | Web search | Notes |
|----------|------|------------|-------|
| **OpenCode CLI** *(default)* | Local Ollama free; cloud per your OpenCode plan | Yes — CLI `--auto` on app-update checks | **No API key in Eir** — uses the logged-in `opencode` CLI. Pick `ollama/<model>` for local Ollama, or any `provider/model` from `opencode models`. |
| **Claude CLI** | Uses your Claude plan | Yes — CLI built-in | **No API key** — reuses your logged-in `claude` session; profile and binary auto-detected. |
| **Codex CLI** | Uses your ChatGPT plan | Yes — CLI built-in | **No API key** — reuses your `codex login` session; binary and model catalogue auto-detected. Launched with your desktop-user token, never as LocalSystem. |
| **Cursor CLI** (`agent`) | Uses your Cursor plan | Ask-mode (read-only) | **No API key** — uses the logged-in Cursor `agent` CLI. Run `agent login` once. |

The monitoring loop and the **app-update check** both use your configured provider.
App-update checks use live web search where the provider supports it (OpenCode `--auto`,
Claude/Codex built-in search). Cursor runs in ask mode.

Settings includes a **Test provider** button that sends a real request through the
service and reports its correlated result. Other UI commands also wait for their
matching service outcome, so applied, rejected, disconnected, and timed-out actions
are reported instead of being silently treated as queued.

## Features

- **Autonomous diagnosis & repair** of common Windows faults, root-cause first —
  reversible fixes run automatically, no babysitting.
- **Tunable autonomy** — set the auto-fix confidence threshold in Settings (default
  80%): lower to act on weaker hunches, higher to be more cautious.
- **Approval backstop** — registry resets and disruptive or irreversible actions
  (closing a program, deleting a file, boot-config edits, driver disabling,
  arbitrary PowerShell) always require your say-so; they're never auto-run. Each
  pending action shows a plain-English summary of what it does, whether it can be
  undone, and — for a file delete — the target's real size, last-modified date, and
  likely kind (regenerable cache vs. irreplaceable data). The approval queue is
  persistent: pending items survive restarts, while an interrupted accepted item
  requires a fresh click instead of being executed again automatically.
- **Never-uninstall guarantee** — software removal is a hard-blocked action.
- **Machine-pattern learning** — repeated local evidence teaches Eir which app-update
  paths, signals, or fixes are not useful on this machine. Learning is conservative,
  decays/rechecks over time, and is fully user-overridable from the tray UI.
- **Reacts as errors land** — signal collectors wake the decision loop the moment an
  error appears (debounced ~10 s, at most once a minute), so fixes start in seconds
  instead of on the next scheduled sweep.
- **Sees the errors you see** — the tray spots classic error message boxes and apps that
  stop responding ("Not Responding") within a few seconds and hands them to the service
  as a high-priority signal. The dashboard's **What Eir noticed** card lists every error,
  failing app log, on-screen message and frozen app as it happens, each with **Explain**
  and **Fix** buttons. Switch it off in Settings → *Watch on-screen errors*.
- **Investigate & fix on demand** — describe a problem in Ask Eir (or press Fix beside a
  noticed error) and Eir runs a focused analysis straight away, reports what it found in
  the chat, and applies fixes through the same safety policy (disruptive ones still wait
  in Approvals).
- **Explains the system** — Ask Eir knows this PC's Windows edition and build, hardware,
  live health, recent errors and how Eir itself works, so it can explain what a service,
  error code or setting does, why something happened, and what Eir is doing about it.
- **Advisor mode** — optional bounded escalation that lets Eir re-run one analysis at
  a stronger model or higher reasoning effort when the base model flags ambiguity or
  reports low confidence. A hard cap of 24 escalations per day keeps it bounded; spend
  remains visible but is not a policy gate.
- **App updates, applied for you** — one panel updates everything. `winget`-managed
  apps update in a single batch; apps no package manager tracks are handled by the
  AI: it finds the official installer via web search, and Eir validates it
  (https-only, trusted-host/vendor-domain gating, `.exe`/`.msi` only, size-bounded
  download, SHA-256 + Authenticode recorded), installs it silently, and **verifies
  the new version is actually installed**. Each result is shown as Current / Verified /
  Installed (unverified) / Failed / Skipped, with method, versions, and available
  signature or failure evidence; recent attempts retain category, exit code, detail,
  and time. Partial inventories, deferred checks, and failed empty runs are shown as
  warnings with their notes, never as “No updates found”. One **⬆ Update everything**
  button does the lot; per-app notes still let you correct or silence false positives
  for your own self-built apps.
- **Usage transparency** — shows AI calls, tokens, and estimated cost in **GBP**.
  Free models are clearly marked as no-cost.
- **Self-updating** — signed auto-updates via the GitHub releases feed.
- **Bounded at every ingress** — AI responses, Ask attachments/history, pipe frames,
  status projections, configuration collections, command output, and updater evidence
  all have explicit limits, so a bad provider or local client cannot grow service
  memory without bound.
- **Stays out of the way** — closing the window hides to the tray; the service keeps
  running. Quit the tray app and the service idles (no AI analysis or updates)
  until it opens again, unless `require_tray = false` is set for a headless PC. The tray app can start with Windows and launch hidden.

## Install

1. Download **`Eir_<version>_x64-setup.exe`** from the
   [latest release](https://github.com/Swatto86/eir/releases/latest).
2. Run it **as Administrator**. The installer registers and starts the `EirSvc`
   service in a protected Program Files directory and seeds the default config. It
   uses the machine **Evergreen WebView2** runtime (already present on most Windows
   10/11 PCs with Edge); if missing, the installer downloads Microsoft’s small
   WebView2 bootstrapper.
3. Launch **Eir** from the Start Menu — the tray icon appears once the service
   connects.
4. Open **Settings** and pick a provider. Default is **OpenCode CLI** — set a
   model such as `ollama/<name>` for local Ollama, or any `provider/model` from
   `opencode models`. **Claude CLI**, **Codex CLI**, and **Cursor CLI** (`agent`)
   reuse your logged-in subscriptions and need no key in Eir.

Already installed? Eir updates itself automatically.

The release also provides a single-file portable tray executable. It needs no
installer or administrator rights, and uses the same Evergreen WebView2 runtime as
other desktop apps (plus no Visual C++ redistributable). It runs EirSvc under the
launching user's token for that session. One portable instance may run
per Windows session and can coexist with an installed Eir. Its config, policy, audit
database, and logs persist under `%LOCALAPPDATA%\EirPortable`; closing the portable UI
also stops its foreground service. Portable mode never changes Start-with-Windows and
never launches the NSIS self-updater—download a newer portable release to update it.
LocalSystem repairs and continuous background monitoring still require the full installer.

After an upgrade, open **About** and confirm the UI and service show the same version.
Then use **Settings → Test provider** to exercise the saved provider through the installed
LocalSystem service. In **App Updates**, “last run” means a cycle completed; “last clean”
means the cycle recorded no source/check/app failure or deferred candidate. It is not proof
that every configured package manager was available.

## Configuration

All settings live in the in-app **Settings** panel: start-with-Windows, AI provider
and models, advisor escalation, polling intervals, watched event-log
channels and directories, and app-updater settings. Provider/monitoring settings are
persisted to `config.toml` next to the installed service executable, or under
`%LOCALAPPDATA%\EirPortable` in portable mode. Provider/model changes apply live; only
collector channel, interval, or directory changes restart the installed service.
Portable mode saves those collector changes and asks you to restart portable Eir.
Updater/advisor settings also apply live.

`config.toml.example` documents every field for reference, but you should never need
to edit it by hand.

## Building from source

Requirements: **Rust** (stable, MSVC toolchain), **Tauri CLI**, and Windows.

```powershell
# 1. Tauri CLI (once)
cargo install tauri-cli --version "^2"

# 2. (Optional) regenerate the icons
powershell -NoProfile -File icons\gen-icon.ps1

# 3. Build the installer. This runs build-svc.ps1 first (which compiles EirSvc
#    and stages bin\eir-svc.exe), then bundles the tray app + service into NSIS.
#    WebView2 uses the machine Evergreen runtime (downloadBootstrapper if absent).
cargo tauri build --config eir-ui/tauri.conf.json -- --locked
```

Run the repository gate (including manifest/Cargo.lock version agreement and locked
Rust builds):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1
```

### End-to-end suite

`e2e/` is a WebdriverIO + `tauri-driver` suite that drives the real debug
`eir.exe` + `eir-svc.exe` through the real webview and named pipe — boot,
"What Eir noticed" from a real injected error dialog, Explain, Investigate &
fix, settings persistence across a restart, and a clean exit. It runs fully
isolated from an installed Eir: a random portable pipe
(`EIR_PORTABLE=1` / `\\.\pipe\EirSvcPortable-<random>`), its own temp state
under an overridden `LOCALAPPDATA`, and a fake `claude_cli` binary
(`e2e/fixtures/fake-claude.cmd`) that never contacts a real model and always
reports zero problems, so the real fix executor can never act on the machine
running the suite. See `e2e/service.ts` for how the isolation is enforced.

Requirements: `tauri-driver` (`cargo install tauri-driver --locked`) and a
`msedgedriver` matching the installed WebView2 Runtime version (Microsoft
signature verified before use).

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-e2e.ps1
```

It's also its own step in `scripts\verify.ps1` (after `cargo test`) and in
CI's `windows-latest` job. `scripts\run-e2e.ps1` never downloads either
WebDriver tool itself — it only locates and verifies what's already on the
machine. CI has nothing pre-installed, so it provisions both itself first:
`scripts\setup-tauri-driver.sh` (a pinned, hash-verified `tauri-driver`) and
`scripts\setup-msedgedriver-ci.ps1` (a WebView2-version-matched,
Microsoft-signature-verified `msedgedriver`).

For v0.34.6 and later, the tag workflow also requires the exact tag
`v<manifest-version>`, reruns its gates from that tag SHA, and keeps the release draft
until the exact installer `.sig` and `latest.json` version, URL, and signature agree.

## Linux (headless)

`eir-svc` also builds and runs as a headless guardian on Linux under systemd — no
tray, no named pipe. The decision loop, policy gate, approvals, learning, audit DB,
and Ask/Investigate are the same OS-neutral code as Windows; every OS-facing edge
(collectors, executor, AI-CLI launch, control-plane transport) has a Linux
implementation alongside the Windows one. `eir-ui` (the Tauri tray) is never built on
Linux — control is entirely through `eirctl`, a small companion CLI (workspace crate
`eir-cli`) that talks the exact same `UiRequest`/`ServiceMsg` JSON-line wire protocol
as the Windows tray, over a Unix domain socket instead of a named pipe.

**What's different from Windows:**
- **Collectors**: `journald` (polled, not streamed) replaces the Windows Event Log;
  `/proc`, `systemctl`, and `ip -j addr show` replace WMI/registry reads. There is no
  Linux analogue of the Windows Firewall/Defender/Windows-Update fields — they stay at
  their "unknown" defaults.
- **Fixes**: only `service_restart`/`service_stop`/`service_start` (via `systemctl`),
  `log_cleanup`, `disk_cleanup` (`apt-get clean` / `journalctl --vacuum-time` / a
  bounded `/tmp` purge), `process_kill` (exact-name, via `/proc`), and `file_delete`
  have real Linux implementations. Every other `FixAction` (registry, scheduled tasks,
  drivers, BCD, Windows Firewall/Defender, SFC/DISM, startup entries) is hard-blocked
  on Linux, both in the AI's own prompt and in `policy.linux.toml`.
- **Policy defaults are stricter**: the Linux auto-execute whitelist is empty — every
  fix, including log and disk clean-ups, needs a human `eirctl approve` first, unlike
  Windows' whitelisted `service_restart`/`stop`/`start`. Add action names to the
  `[whitelist]` in `/etc/eir/policy.toml` to opt in.
  See CONTEXT.md's decision entry for why.
- **Protected units**: a two-layer backstop independent of `policy.toml` guards
  `systemctl stop`/`restart` (never `start`, which is not disruptive) on core system
  units — a compiled list (`ssh.service`, `tailscaled.service`, `systemd-*`, `docker`,
  `eir.service` itself, …) plus an optional, additive, host-specific drop-in at
  `/etc/eir/protected-units.d/*.conf` (one glob per line; ships empty by default).
- **AI CLI privilege drop**: when `eir-svc` runs as root (the systemd unit's default),
  it never launches the AI CLI as root. `[api] linux_ai_user` names the local, non-root
  account whose CLI login is used; the CLI child keeps only that account's primary
  group (docker, sudo, adm and every other supplementary group are dropped) and runs
  with no-new-privileges, so even a passwordless sudo rule cannot raise it. An ordinary
  admin login is therefore fine. Leaving it unset disables the AI subsystem rather than
  running the CLI as root — collectors, the executor and approvals still run.
- **Logs** go to the journal: `journalctl -u eir`.
- **Real time**: a unit that crashes is an immediate trigger (systemd's "unit result"
  journal event), each analysis re-reads the failed-unit list and includes the recent
  log of every failed unit, so the AI sees why it failed.

### Install

```bash
# 1. Build (from this repo, on Linux)
cargo build --release --locked -p eir-svc -p eir-cli

# 2. Install the binaries, config and policy
sudo install -m 755 target/release/eir-svc /usr/local/bin/eir-svc
sudo install -m 755 target/release/eirctl  /usr/local/bin/eirctl
sudo mkdir -p /etc/eir /etc/eir/protected-units.d   # /var/lib/eir: eir.service's own StateDirectory= creates it
sudo install -m 640 config.toml.linux.example /etc/eir/config.toml   # then edit it
sudo install -m 640 policy.linux.toml          /etc/eir/policy.toml

# 3. Install and start the service
sudo install -m 644 packaging/systemd/eir.service /etc/systemd/system/eir.service
# The AI CLI writes its scratch workspace and login state in linux_ai_user's home,
# which the unit's sandbox (ProtectHome=read-only) otherwise blocks:
sudo mkdir -p /etc/systemd/system/eir.service.d
printf '[Service]\nReadWritePaths=/home/ubuntu/.cache -/home/ubuntu/.claude -/home/ubuntu/.claude.json\n' \
  | sudo tee /etc/systemd/system/eir.service.d/10-ai-user.conf   # use your linux_ai_user's home
sudo systemctl daemon-reload
sudo systemctl enable --now eir
systemctl status eir
```

At minimum, edit `/etc/eir/config.toml`'s `[api]` section: set `provider` and
`linux_ai_user` to a real, already-logged-in local account (e.g. `claude login` /
`codex login` / `opencode auth login` run once as that user). Then drive it with
`eirctl` (as that user, or any uid/gid listed in `[service] socket_allow_uids`/
`socket_allow_gids` — root is always allowed):

```bash
eirctl status               # live metrics, failed units, pending approvals
eirctl approvals             # actions awaiting a human decision
eirctl approve <id>          # or: eirctl reject <id>
eirctl pause                 # pause monitoring (eirctl resume to continue)
eirctl ask "what is eating memory right now"
eirctl investigate "check for anything unusual"
```

`eirctl --json <command>` emits the raw decoded payload for scripting. Set `$EIR_SOCKET`
to point at a non-default `service.socket_path`.

## Project layout

| Crate | Layer | Responsibility |
|-------|-------|----------------|
| `eir-proto` | shared | Wire types for the UI ↔ service pipe/socket protocol. |
| `eir-svc` | service | LocalSystem/root service: signal collection, AI client, policy, execution, audit DB. |
| `eir-ui` | presentation | Tauri tray app; static frontend in `ui/`. Windows only. |
| `eir-cli` (binary `eirctl`) | presentation | Linux control CLI over the Unix control socket. |

## Security model

- The service runs as **LocalSystem**; the UI runs at **Medium** integrity (normal
  user). They communicate only over the local named pipe `\\.\pipe\EirSvc`.
- Service installation fails closed unless `eir-svc.exe` is an ordinary, single-link
  file directly under a protected Program Files directory. Install and upgrade reject
  redirected paths, copy legacy state from a held source stream into a fresh protected
  file, reset ownership/ACLs on Eir-owned files, and restore the previous service after
  an interrupted in-place upgrade. Uninstall removes only validated Eir state without
  following reparse points.
- The pipe is created with an explicit security descriptor —
  granting Interactive Users only the access needed to exchange data while a Medium
  mandatory label blocks Low-integrity clients. The service accepts only the installed
  sibling UI (or an elevated administrator) in the sole active interactive session,
  and rechecks that session during I/O; the UI independently verifies that the pipe
  server is the registered LocalSystem `EirSvc`. Portable clients deliberately cannot
  control the privileged service. No network listener is opened.
- Portable mode uses a random private pipe and mutually verifies that the UI and
  foreground service are same-user, same-session sibling processes. The service rejects
  LocalSystem and split-token/full-elevation portable launches, and a delete-on-close
  runner lease shuts it down if the runner exits.
- Destructive actions are blocked at the policy layer and require explicit approval;
  software uninstalls are never permitted.
- User-owned files, Winget, and OpenCode/Claude/Codex/Cursor sessions are scoped to the sole active
  desktop user and accessed with that user's Windows token. Multiple active sessions
  fail closed; user-controlled reparse paths are rejected.
- Updater and executor boundaries reject ambiguous installed identities, unsafe
  installer paths/arguments, protected process targets, and unverifiable/no-effect
  operations instead of reporting them as successful.
- API keys are stored in the local `config.toml` and never logged.

## License

MIT © Swatto
