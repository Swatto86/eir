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
                                                       │  OpenRouter /    │
                                                       │ Claude / Codex / │
                                                       │  Kilo / Ollama   │
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

Six providers:

| Provider | Cost | Web search | Notes |
|----------|------|------------|-------|
| **OpenRouter** *(default)* | Free models | Yes — web plugin | Recommended. `openrouter/free` auto-routes to a current free model; needs an API key. |
| **Claude CLI (your subscription)** | Uses your Claude plan | Yes — CLI built-in | **No API key** — reuses your logged-in `claude` session; profile and binary auto-detected. |
| **Codex CLI (your subscription)** | Uses your ChatGPT plan | Yes — CLI built-in | **No API key** — reuses your `codex login` session; binary and current model catalogue are auto-detected. The service launches it with your desktop-user token, never as LocalSystem. |
| **Claude (Anthropic API)** | Pay-as-you-go | Yes — native web_search tool | API key from console.anthropic.com; token usage tracked, cost estimated from list pricing. |
| **Kilo Code — your subscription (Kilo CLI)** | Uses your Kilo plan (Pass + addon BYOK included) | Yes — the CLI's built-in | **No API key** — borrows your logged-in `kilo` session, same way the Claude CLI borrows a logged-in Claude subscription. Install with `npm install -g @kilocode/cli`, run `kilo` once to sign in, then pick the provider in Settings with a `provider/model` id — **the `kilo/` prefix routes through your subscription/BYOK**, e.g. `kilo/minimax/minimax-m3`. Profile / binary are auto-detected. |
| **Ollama (local)** | Free / local hardware | No | **No API key** — talks to a local Ollama server (default `http://127.0.0.1:11434/v1`). Install Ollama, `ollama pull` a model, then pick it in Settings. Vision works when the model supports it. App-update checks have no web search. |

The monitoring loop and the **app-update check** both use your configured provider.
The app-update check uses live web search where the provider supports it: OpenRouter's
web plugin (works with free models — about £0.004 per check for the search),
Anthropic's native web-search tool, the Claude or Codex CLI's built-in search
(`update_check_model`, blank = a provider default), or the Kilo CLI's own
`--auto` agent-loop search.

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
  running. The tray app can start with Windows and launch hidden.

## Install

1. Download **`Eir_<version>_x64-setup.exe`** from the
   [latest release](https://github.com/Swatto86/eir/releases/latest).
2. Run it **as Administrator**. The installer registers and starts the `EirSvc`
   service in a protected Program Files directory and seeds the default config. It
   includes a pinned WebView2 runtime, so Windows does not need one preinstalled.
3. Launch **Eir** from the Start Menu — the tray icon appears once the service
   connects.
4. The default provider is **OpenRouter**. Open **Settings**, paste your
   [OpenRouter API key](https://openrouter.ai/keys), and Save — that's all it needs
   (the `openrouter/free` model is preset). Prefer Claude? Switch the provider to
   **Claude CLI** or **Codex CLI**, which reuse your logged-in subscription and
   need no key — or use an Anthropic API key (console.anthropic.com) plus a
   model, or your logged-in **Kilo CLI** session.

Already installed? Eir updates itself automatically.

The release also provides a single-file portable tray executable with the same pinned
WebView2 runtime embedded and extracted to a temporary directory while it runs. It needs
no installer, administrator rights, preinstalled WebView2, or Visual C++ runtime: it runs
EirSvc under the launching user's token for that session. One portable instance may run
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
and models, API keys, advisor escalation, polling intervals, watched event-log
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
#    and stages bin\eir-svc.exe), verifies a fresh extraction of the pinned
#    WebView2 CAB, then bundles the tray app + service + runtime into NSIS.
cargo tauri build --config eir-ui/tauri.conf.json -- --locked
```

Run the repository gate (including manifest/Cargo.lock version agreement and locked
Rust builds):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1
```

For v0.34.6 and later, the tag workflow also requires the exact tag
`v<manifest-version>`, reruns its gates from that tag SHA, and keeps the release draft
until the exact installer `.sig` and `latest.json` version, URL, and signature agree.

## Project layout

| Crate | Layer | Responsibility |
|-------|-------|----------------|
| `eir-proto` | shared | Wire types for the UI ↔ service pipe protocol. |
| `eir-svc` | service | LocalSystem service: signal collection, AI client, policy, execution, audit DB. |
| `eir-ui` | presentation | Tauri tray app; static frontend in `ui/`. |

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
- User-owned files, Winget, and Claude/Codex/Kilo sessions are scoped to the sole active
  desktop user and accessed with that user's Windows token. Multiple active sessions
  fail closed; user-controlled reparse paths are rejected.
- Updater and executor boundaries reject ambiguous installed identities, unsafe
  installer paths/arguments, protected process targets, and unverifiable/no-effect
  operations instead of reporting them as successful.
- API keys are stored in the local `config.toml` and never logged.

## License

MIT © Swatto
