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
  problems and executions, AI usage/cost, and app updates. It's where you approve
  fixes and change every setting.

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
                                                       │  OpenRouter /    │
                                                       │  Claude / API    │
                                                       └──────────────────┘
```

Each decision cycle (default every 10 minutes):

1. **Collect signals** — Windows Event Log channels, service states, CPU/memory/disk,
   network errors, watched log directories.
2. **Decide whether to think** — Eir only calls the AI when something *actionable*
   has changed (a fingerprint of the current problems), plus a periodic heartbeat so
   a healthy machine still reports in. Idle cycles are essentially free.
3. **Diagnose** — the AI returns a structured list of problems, each with a
   confidence score and a proposed root-cause fix.
4. **Gate through policy** — low-confidence findings and benign Windows noise are
   dropped; blocklisted actions (e.g. uninstalling software) are *never* executed,
   even with approval.
5. **Execute or ask** — safe, reversible fixes can run automatically; anything risky
   waits for your approval in the tray UI.

## AI providers

Everything is configurable in the **Settings** panel — no file editing required.

| Provider | Cost | Web search | Notes |
|----------|------|------------|-------|
| **OpenRouter** | Free models available | No | Recommended default. Free models cost nothing and don't touch your Claude plan. |
| **Claude CLI** | Uses your Claude plan | Yes | No API key — reuses your logged-in `claude` session. |
| **Anthropic API** | Pay-as-you-go | Yes | Direct API key from console.anthropic.com. |
| **OpenAI-compatible** | Depends | Depends | Any OpenAI-style endpoint (e.g. a local proxy). |

The main monitoring loop can run on a cheap/free model. The **app-update check** (see
below) needs live web search, so it always runs through the Claude CLI on a Claude
model (defaults to **Haiku**, the cheapest).

## Features

- **Autonomous diagnosis & repair** of common Windows faults, root-cause first.
- **Approval workflow** — risky actions never run without your say-so.
- **Never-uninstall guarantee** — software removal is a hard-blocked action.
- **App update monitoring** — surfaces available updates via `winget`, and uses AI
  web search for apps no package manager tracks. Per-app notes let you correct or
  silence false positives (e.g. for your own self-built apps).
- **Usage transparency** — shows AI calls, tokens, and estimated cost in **GBP**.
  Free models are clearly marked as no-cost.
- **Self-updating** — signed auto-updates via the GitHub releases feed.
- **Stays out of the way** — closing the window hides to the tray; the service keeps
  running.

## Install

1. Download **`Eir_<version>_x64-setup.exe`** from the
   [latest release](https://github.com/Swatto86/eir/releases/latest).
2. Run it **as Administrator**. The installer registers and starts the `EirSvc`
   service and seeds the default config.
3. Launch **Eir** from the Start Menu — the tray icon appears once the service
   connects.
4. Open **Settings** and pick your AI provider. The free OpenRouter option needs an
   [OpenRouter API key](https://openrouter.ai/keys); the Claude CLI option needs a
   logged-in `claude` session and no key.

Already installed? Eir updates itself automatically.

## Configuration

All settings live in the in-app **Settings** panel: AI provider and models, API
keys, polling intervals, watched event-log channels and directories. The service
persists them to `config.toml` next to its executable and restarts to apply.

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
cargo tauri build --config eir-ui/tauri.conf.json
```

Run the checks the way CI does:

```powershell
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

## Project layout

| Crate | Layer | Responsibility |
|-------|-------|----------------|
| `eir-proto` | shared | Wire types for the UI ↔ service pipe protocol. |
| `eir-svc` | service | LocalSystem service: signal collection, AI client, policy, execution, audit DB. |
| `eir-ui` | presentation | Tauri tray app; static frontend in `ui/`. |

## Security model

- The service runs as **LocalSystem**; the UI runs at **Medium** integrity (normal
  user). They communicate only over the local named pipe `\\.\pipe\EirSvc`.
- The pipe is created with an explicit security descriptor —
  `D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)S:(ML;;NW;;;ME)` — granting authenticated
  users read/write while a Medium mandatory-label SACL lets the Medium-integrity UI
  write to it (no-write-up). No network listener is opened.
- Destructive actions are blocked at the policy layer and require explicit approval;
  software uninstalls are never permitted.
- API keys are stored in the local `config.toml` and never logged.

## License

MIT © Swatto
