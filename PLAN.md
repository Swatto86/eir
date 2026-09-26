# Eir roadmap — v0.36.0 candidate

**Release line:** v0.35.0 (published 2026-09-24 from 3b3837a; Windows assets only)

**Current code:** v0.36.0 candidate on `master`, not yet published: the headless Linux build
(`eir-svc` + `eirctl`) and the fixes below, all found in three weeks of the installed
v0.34.18–v0.35.0 service's own log and audit database on the owner's PC.

## What v0.36.0 adds

- "What Eir noticed" has **Clear** and a per-item **Dismiss** (`ClearNoticed`,
  capability-gated). Display-only: nothing recorded is deleted.
- The log watcher reports only what a log newly wrote, once per line per 6 h, and never
  reads structured state files, the user's Temp folder or Eir's own folders. It had been
  starting an AI analysis every 2–3 minutes on one harmless Discord line (4,366 of 4,390
  analyses found nothing; ~10 M input tokens a day), and its dropped-event warnings made
  up 81% of a 61 MB service log.
- App updates: apps with no known installed version (Battle.net, which failed every day)
  and apps installed only for a user (the owner's unsigned per-user Tauri apps, Electron
  apps) are left to update themselves and named in notes; an app that fails in two runs
  in a row is paused for 2, then 4, then 7 days, with Retry to try at once; a Chocolatey
  package and its `.install` variant are one candidate.
- Safety: a PowerShell script can no longer be always-approved. One such approval had
  covered every future AI-written script run as SYSTEM; a stored grant is removed at
  service start.
- Game Mode holds for up to 10 minutes out of fullscreen while the game still runs, instead
  of dropping after 60 s and starting deferred analysis mid-session.
- Hung-window reports skip invisible helper windows (every Tauri app's `-siw` window,
  Discord's overlay).

The only protocol change is the additive, capability-gated `ClearNoticed`: with a v0.35.0
tray or service on either end, Clear and Dismiss are simply not shown.

## What v0.35.0 adds

- On-screen errors: the tray reports classic error message boxes and hung windows
  (`ReportScreenError`), a fourth signal source that triggers the reactive path.
- Dashboard "What Eir noticed" feed with Explain / Fix per item.
- Investigate & fix: a user-described problem runs one focused analysis; fixes go through
  the unchanged policy gate and the outcome is posted to Ask Eir.
- Ask Eir explains the system: machine profile, live details, recent errors and how Eir
  works.
- Fix: collector buffers are no longer drained and discarded by a scheduled tick that lands
  during an in-flight analysis.
- Fix: OpenCode sessions are deleted after every run (they had grown the user's
  opencode.db by gigabytes).
- WebDriver end-to-end suite (`e2e/`) in the local gate, CI and the release workflow.

None of this adds repair authority: every fix still comes from the same action catalogue and
policy gate.

Pre-publish evidence (2026-09-24): the packaged v0.34.19 → v0.35.0 upgrade was installed on
the owner's PC (service running, tray launched from the Start Menu and connected, a real error
box reported by the installed tray); a portable live pass with a real provider analysed an
on-screen error and a hung window; the e2e suite covers Explain and Investigate & fix.

## Headless Linux build (on `master`, unreleased)

- `eir-svc` builds and runs on Linux under systemd with no tray. `eirctl` (directory
  `eir-cli`, package `eirctl`) drives it over a Unix socket whose peer credentials are
  checked: status, approvals, approve/reject, pause/resume, ask and investigate.
- journald, `/proc`, `systemctl` and `ip` replace the Windows collectors. A crashed unit
  triggers an analysis at once, and every analysis carries failed units' recent logs.
- Deliberately stricter than Windows: 7 fix types, an empty auto-execute whitelist (every
  fix waits for `eirctl approve`), a protected-units backstop, and the AI CLI dropped to a
  non-root `linux_ai_user` with only its primary group and no-new-privileges.
- Built from source only (README "Linux (headless)"); CI's `verify-linux` job gates it.
  Running on swatbox (systemd, root) and swatbot (container without systemd, as an
  ordinary user).

The Windows build, policy and behaviour are unchanged by it.

Not done, on purpose, until there is a reason: Linux release artifacts, log-directory
watching on Linux, and `[notify]` call sites (these need Swatto's go-ahead before anything
messages an external chat).

## Next work

Keep the next work narrow and evidence-led:

1. Anchor native-updater publisher identity in signed installed software or a curated
   local mapping instead of an AI claim.
2. Add an explicit config schema version and preserve unknown keys before the format
   evolves further.
3. Add guarded durable undo receipts for startup and task toggles only when their live
   state can be compared safely with Eir's applied state.

Do not add remote control, a plugin system, general-purpose shell authority, new repair
families, or policy tuning that can expand automatic authority.

## Release gate

Every behavioural fix starts with a failing regression check. A candidate is ready only
when the full local gate, packaged upgrade, real WebView workflow, standalone executable
smoke, and exact-SHA CI all pass.

Publication follows the repository's mandatory order:

1. Push the release commit and wait for CI to pass on that exact SHA.
2. Apply the exact `v<manifest-version>` tag. The tag workflow must check out that SHA
   and rerun its gates; it may publish only after the exact installer `.sig`, updater
   metadata version/tagged URL/signature, smoke-tested portable executable, and
   checksums agree.
