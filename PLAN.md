# Eir roadmap — v0.34.6

**Release line:** v0.34.6

**Current code:** v0.34.6

## v0.34.6 release gate

The implementation sweep and local release gate are complete. The full repository gate,
real NSIS build, and live self-contained portable workflow passed on Windows. Publication
still follows the repository's mandatory order:

1. Push the release commit and wait for CI to pass on that exact SHA.
2. Apply the exact
   `v<manifest-version>` tag. The tag workflow must check out that SHA and rerun its
   gates; it may publish only after the exact installer `.sig`, updater metadata
   version/tagged URL/signature, smoke-tested portable executable, and checksums agree.

CI owns the elevated v0.34.5→v0.34.6 Program Files/LocalSystem upgrade check that cannot
be completed unattended in a non-elevated local session.

## What v0.34.6 closes

- Service installation and upgrade are confined to protected Program Files paths,
  reject reparse/hardlink tricks, hold each validated migration source against mutation
  through the copy, and retain a recoverable prior service across an interrupted
  in-place upgrade. Uninstall removes only validated Eir paths without traversing
  reparse points.
- Privileged user work is bound to the sole active desktop session. CLI providers,
  Winget, file discovery, and disk scans no longer search or traverse unrelated profiles
  as LocalSystem.
- The named pipe authenticates both peers and bounds inbound/outbound frames. AI/Ask,
  status, config, audit, subprocess, and updater data also have explicit size/count limits.
- Approval recovery is fail-safe: an accepted row interrupted before durable completion
  returns to pending for fresh approval instead of replaying automatically.
- Updater/executor paths fail closed on ambiguous app identity, unsafe installer or
  verification paths, protected process targets, no-effect operations, and failed
  uninstall exit codes.
- Installer and portable artifacts carry a pinned, hash-checked, signed fixed WebView2
  runtime. Only the CAB is cached; each build re-extracts and authenticates a fresh
  runtime before packaging. The portable is a single self-extracting executable with a
  one-per-session, default-token foreground service, a random mutually authenticated
  same-user pipe, runner-liveness shutdown, and persistent
  `%LOCALAPPDATA%\EirPortable` state. It does not alter autostart or invoke the installed
  updater/restart paths. Static CRT linkage plus an import gate keeps unshipped MSVC and
  WebView2 loader DLLs out of its runtime prerequisites.

## After v0.34.6

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
