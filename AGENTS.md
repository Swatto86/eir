# Eir

Eir is an autonomous system-repair agent written in Rust. On Windows a LocalSystem service
(`eir-svc`) watches event logs, services, disk, memory and on-screen errors, asks an AI CLI
(OpenCode, Claude, Codex or Cursor) to diagnose them, and applies fixes behind a policy gate;
a Tauri v2 tray app (`eir-ui`) shows status, approvals and settings over a secured named pipe.
The same service also builds for Linux, where it runs headless under systemd and is driven by
the `eirctl` CLI over a Unix socket. The release line is v0.35.0 (Windows assets only); the
Linux build is on `master`, unreleased and built from source.

## Layout

| Path | What it is |
|---|---|
| `eir-proto/` | Shared serde wire types for the pipe/socket JSON-line protocol |
| `eir-svc/` | The service: signals, AI client, policy, executor, app updater (Windows), SQLite audit DB |
| `eir-ui/`, `ui/` | Tauri tray app, Windows only; `ui/` is hand-written HTML/JS, committed, no npm or bundler |
| `eir-cli/` | Package and binary `eirctl` (Linux control CLI; a stub on Windows), so Cargo uses `-p eirctl` |
| `e2e/` | WebdriverIO + tauri-driver end-to-end suite (the only `package.json`) |
| `migrations/` | sqlx migrations for the audit DB |
| `policy.toml`, `config.toml.example` | Windows policy and config template (the installer seeds config from it) |
| `policy.linux.toml`, `config.toml.linux.example`, `packaging/systemd/eir.service` | The Linux equivalents |

`eir-ui/tauri.conf.json` is the canonical build config. `eir-ui/bin/eir-svc.exe` is a
gitignored build artifact staged by `eir-ui/build-svc.ps1`.

## Build, test and verify

- Rust is pinned to 1.95.0 (`rust-toolchain.toml`; CI matches). Cargo commands use `--locked`.
- Fast check: `pwsh scripts/fastcheck.ps1` (optionally `-Package eir-svc|eir-proto|eir-ui`).
- Full Windows gate: `pwsh scripts/verify.ps1` runs version sync, installer/release/portable
  regressions, fmt, clippy, tests, a release build, `cargo deny` and the WebDriver e2e suite
  (`scripts/run-e2e.ps1`, which needs `tauri-driver` and a WebView2-matched `msedgedriver`).
  It opens real windows and error dialogs and saturates the CPU: on SwatPC, check that Swatto
  is not using the PC before running it.
- Linux: `cargo clippy --locked -p eir-proto -p eir-svc -p eirctl --all-targets -- -D warnings`
  and the matching `cargo test`. Never `--workspace` there: the tray pulls GTK, which is not
  installed.
- CI (`.github/workflows/ci.yml`): Windows `verify` (the gate above plus a signed Tauri build and
  LocalSystem/portable smokes), `verify-linux`, and a dependency audit. A release commit must be
  green on its exact SHA before the `v<version>` tag starts `release.yml`.
- Versions move together across five manifests (the four crates' `Cargo.toml` and
  `eir-ui/tauri.conf.json`) and four `Cargo.lock` entries; `scripts/check-versions.ps1` gates
  all nine.

## Constraints

- No new repair authority: fixes come only from the `FixAction` catalogue through the policy
  gate; learning may only skip, deprioritise or lower confidence; software uninstall is always
  blocked. Linux ships an empty auto-execute whitelist and a protected-units backstop.
- Wire compatibility: tray and service can run different versions during an update, so new
  `eir-proto` fields are additive with `#[serde(default)]`, removed ones stay as deprecated
  fields, and new commands are capability-gated.
- AI providers are CLIs only and Eir stores no API keys. On Linux the CLI runs as
  `[api] linux_ai_user`, never as root.
- Every behavioural fix starts with a failing regression test.
- Publishing a release needs Swatto's go-ahead. Never configure the Linux `[notify]` hook on a
  host without it.

## Open questions

- Move learning thresholds, windows and half-lives into config once the detectors have more
  real-world history.
- Tune the resource-trend thresholds and disk-health/SMART wording against real machine history.
- Confirm the `network_errors` CIM query resolves on target machines, or drop the field.
- Linux: publish release artifacts or stay source-only; add log-directory watching
  (`signals::file_watch` is a stub there); wire `[notify]` call sites only with Swatto's approval.

## Where longer material lives

- `README.md`: the user guide, install, the Linux (headless) walkthrough and the security model.
- `ARCHITECTURE.md`: the deep technical reference. Update it in the same commit as a behaviour
  change; read only the sections a task needs.
- `CONTEXT.md`: durable decisions and releases, newest first, each with its reason.
- `PLAN.md`: release line, roadmap and release gate.
- The public page is `content/projects/eir.md` in the swatto.co.uk repo; keep it in step with
  user-facing features and platforms.
- Live installs on SwatPC, swatbox and swatbot are recorded in agent-memory host facts.
