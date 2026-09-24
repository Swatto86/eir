# Eir roadmap — v0.35.0

**Release line:** v0.34.19 (published)

**Current code:** v0.35.0 (local release candidate; not tagged or published)

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

## v0.34.17 release gate

Publication follows the repository's mandatory order:

1. Push the release commit and wait for CI to pass on that exact SHA.
2. Apply the exact `v<manifest-version>` tag. The tag workflow must check out that SHA
   and rerun its gates; it may publish only after the exact installer `.sig`, updater
   metadata version/tagged URL/signature, smoke-tested portable executable, and
   checksums agree.

## What v0.34.17 ships

- Approvals: reversible **Ignore** and **Always Approve** for a semantic fix
  (`FixAction::dedup_key`), managed from the Learned preferences list.
- App Updates: Ignore removes an app from Updates Available immediately; Unignore
  remains in Settings.
- Learned: RejectedSignal counts stable action keys so repeated rejects can form a
  fact; UI clarifies automatic learning vs hard preferences.
- Build: remove invalid Cargo `jobs = 0` so Rust 1.95 CI/release builds succeed.

## After v0.34.17

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
smoke, and exact-SHA CI all pass. CI must pass before the tag that starts publication.
