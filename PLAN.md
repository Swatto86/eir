# Eir — correctness & hardening bug-fix plan C1–C21 (handover to Opus)

**Baseline:** v0.24.1 (`aa97d7b`), tree clean, synced with `origin/master`.
**Theme:** correctness, safety, and security-hardening bugs found in a fresh
multi-agent adversarial sweep (service loop, AI layer, updater, signals/executor,
persistence/learn, frontend). Every item was traced against the source at this
baseline and the load-bearing ones were independently re-verified; file/line
references are to this tree. No new features — the smallest correct fix in each
case. Six subsystems were swept by parallel agents (several by two independent
agents, which converged on the P1/P2 findings).

**Ground rules (apply to every item):**

- The frontend stays committed static vanilla HTML/CSS/JS (`ui/index.html`,
  `ui/main.js`) — no npm, no new JS dependencies. All service-supplied strings
  rendered into HTML go through `esc()`/`escAttr()`.
- The UI never constructs a `FixAction`; commands stay opaque ids over the pipe.
  Nothing here widens the pipe trust model.
- No wire-shape changes are required. Any new `serde` field on a wire type must be
  `#[serde(default)]` to preserve the forward/backward-compat skew invariant.
- Rust changes get a unit test where the logic is testable without a live service
  (the pure gates: C2 UNC guard, C6 path canonicalisation, C3 fence-stripping,
  C12 tie-break, C16 clamp). Where a change can only be exercised live (C1 drain,
  C5 ACL retry, C11 tray gate) say so in the release notes.
- Gate before release: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, full
  tauri build via CI (`scripts/check-versions.ps1` gates the version sync).
  Adversarial sweep (multi-lens + refute) before tagging, per CLAUDE.md.
- Frontend changes have no automated harness — state plainly in the release notes
  which items are compile/reasoning-verified only.
- **Packaging:** all items in one patch release, **v0.24.2**. Bump the three
  `Cargo.toml`s + `eir-ui/tauri.conf.json`, re-sync `Cargo.lock`, update
  ARCHITECTURE.md + CONTEXT.md in the same `[release]` commit, tag, publish
  (single rolling release).

**Suggested order:** C1 → C2 → C6 → C9 (the safety/loss items) first, then the
rest. C1 is the highest-value fix and the largest diff; do it first while the loop
is freshly in context.

---

## P1 — data loss / security

### C1 — Approved fixes (and their audit trail) are abandoned when the service restarts or stops mid-execution

**What.** The `Approve` handler deletes the pending-approval row from the DB
*before* dispatching the job (`eir-svc/src/main.rs:1641-1642`, then `exec_tx.send`
at `:1657`). Execution runs on the off-loop executor worker, whose `JoinHandle` is
**discarded** (`spawn_executor(&db, exec_rx, exec_done_tx);` at
`eir-svc/src/main.rs:1025`; the fn `tokio::spawn`s and returns `()`,
`:523-529`). Both service-exit paths return straight out of `eir_main` without
draining that worker:

- `UpdateSettings` → `restart_self(); return;` (`:1624-1625`).
- SCM `Stop`/`Shutdown` (and Ctrl-C in dev) → the `shutdown` select arm just logs
  and the fn returns (`:2485-2487`).

When `eir_main` returns, `rt.block_on` returns and the multi-thread Tokio runtime
is dropped — **aborting every still-running task**, including the executor worker
mid-`executor::execute`, mid-`audit::log_execution`, or with jobs still queued in
the unbounded channel. Because the pending row was already deleted, the approved
action now leaves **no `execution_log` row, no `mark_decision_executed`, no
feedback record**, and for a `RegistryReset` the PowerShell write may have already
landed on disk with the undo snapshot never persisted. On next start the UI shows
nothing pending and nothing executed — the approval simply vanished.

This is not just the manual settings-save case: the **self-updater's NSIS hook does
`sc stop EirSvc`** on every auto-update, so any fix executing when an update lands
hits the same abandonment. Long fixes (`SfcScan`/`DismRestoreHealth`, 40 min) are
the widest exposure window.

**Fix.** Drain the executor on the way out.

1. Change `spawn_executor` to return its `tokio::task::JoinHandle<()>` (return the
   `tokio::spawn(...)` instead of discarding it); bind it in `eir_main`
   (`let exec_handle = spawn_executor(...)`).
2. Add a small `drain_executor(exec_tx, exec_handle)` that `drop`s `exec_tx` (so
   the worker's `while let Some(job) = job_rx.recv().await` loop ends once the
   channel is empty) and `await`s the handle under a bounded
   `tokio::time::timeout` (e.g. 30 s — long enough for the queued quick fixes,
   short enough that SCM's stop timeout / a 40-min SFC doesn't wedge shutdown; SCM
   will hard-kill anyway, and that case is unavoidable).
3. Call it before the `restart_self(); return;` in the `UpdateSettings` arm, and in
   the `shutdown` arm before `eir_main` returns. (The `exec_tx` clones used by the
   undo path must also be dropped/out of scope for the channel to close — verify no
   other live `exec_tx` clone keeps the worker's receiver open.)

Root cause is "delete-before-durably-recorded". A fuller fix (mark the approval
`executing` and only delete after `log_execution`, so a crash re-surfaces it) is
out of scope for this patch — the drain closes the common restart/stop window with
a contained change. Note the residual: an abrupt process kill (SCM hard-timeout,
power loss) can still abandon an in-flight job; the drain does not make execution
crash-atomic, and the release notes should say so.

**Verify.** Rust: unit-test `drain_executor` (a job sent then drain completes and
the outcome is observed / the handle joins within the timeout). The runtime-drop
abandonment itself is live-only — flag it.

### C2 — AI-supplied `verify_exe` accepts a UNC path, forcing LocalSystem to authenticate to an attacker SMB share

**What.** For an app with no winget/choco/scoop/msstore coverage, the native
method asks the AI for an install plan and takes `verify_exe` from the response.
`plan.rs:312-315` only trims and filters empty/`"null"` — **no host/scheme/drive
validation**. That value reaches `exe_file_version` (`eir-svc/src/updater/verify.rs:125-142`,
via `VerifyTarget::ByName` when `winget_installed_version_by_name` returns `None`,
which is the norm for a genuinely-unmanaged native app), which runs
`Get-Item -LiteralPath '<path>'` as LocalSystem. The only guard is
`if !Path::new(path).is_absolute()` (`:126`) — and on Windows `is_absolute()`
returns **true for a UNC path** `\\attacker.example\share\x.exe`. Windows then
resolves the UNC over SMB, causing the machine account to attempt NTLM
authentication to the attacker's server — a classic forced-authentication /
NTLM-relay primitive, fully unattended, triggered by AI output (which the plan
prompt explicitly tells the model to source by browsing the web, so a poisoned
"official download" page is a plausible injection vector).

The PowerShell injection itself is already closed (`-LiteralPath`, `'`-doubling) —
the UNC path is the primitive, not the quoting.

**Fix.** In `exe_file_version`, after the `is_absolute()` check, require a
drive-letter prefix and reject UNC: inspect `Path::new(path).components().next()`
and accept only `Component::Prefix(p)` where
`matches!(p.kind(), Prefix::Disk(_))`; return `None` for
`Prefix::UNC`/`Prefix::VerbatimUNC`/`Verbatim`. Two lines, one sink; no change
needed in `plan.rs`. (Optionally also reject in `validate_plan` for defence in
depth, but `verify.rs` is the only reachable execution sink.)

**Verify.** Unit test: `\\host\share\x.exe` and `\\?\UNC\...` → `None`;
`C:\Program Files\App\app.exe` → passes the guard. Reasoning-verified for the
live SMB behaviour (don't exercise it against a real share).

---

## P2 — correctness & security-hardening

### C3 — `strip_fences` throws away a valid AI response, dropping the whole analysis cycle

**What.** `analyze_with` does `let json_text = strip_fences(&raw)` and only ever
parses/falls-back over `json_text` (`eir-svc/src/ai/client.rs:348-360`;
`strip_fences` at `:1462-1480`). `strip_fences` scans the **entire** response for
the first ```` ``` ````/`~~~` fence via `s.find(open)` and slices to the next
close. Two independent failure modes, both confirmed:

- The prompt injects up to ~2.5 KB of raw log/config `content_excerpt`
  (`signals/log_parser.rs`), which can itself contain a triple-backtick block; if
  the model quotes that back inside `"diagnosis"`/`"reasoning"`, the **first**
  backtick run mid-string is treated as the opening fence and the real JSON braces
  before/after are discarded.
- If the model emits an unrelated fenced block before its (untagged) JSON answer,
  the first-fence pick returns the wrong block.

The `extract_json_object` fallback then runs on the already-mangled `json_text`
(`:356`), not `raw`, so it can't recover the real object either. Result:
`Err("Failed to parse model response as JSON")` and the cycle's diagnosis is lost
(self-heals next cycle, but avoidably).

**Fix (both, small).** (a) Run the fallback over the original text:
`let extracted = extract_json_object(&raw);` at `:356`. (b) Make `strip_fences`
only treat a fence at the **trimmed start** of the response as a real fence (e.g.
`s.trim_start().strip_prefix("```json")` / `"```"` / `"~~~..."` per variant),
matching how models actually emit fenced JSON (fence-then-content), so a mid-string
backtick run can never be mistaken for a fence. Either fix alone recovers most
cases; do both.

**Verify.** Unit tests: a response whose `diagnosis` string contains a ```` ``` ````
block still parses; a bare `{...}` with no fence still parses; a genuinely
fenced ` ```json {...} ``` ` still parses.

### C4 — The autonomous updater trusts any GitHub release host for any app, with no repo↔app correlation

**What.** `host_acceptable(host, name)` returns `true` as soon as
`host_trusted(host)` is true, **before** any name check
(`eir-svc/src/updater/plan.rs:139-141`). `TRUSTED_HOSTS` includes `github.com`,
`objects.githubusercontent.com`, `release-assets.githubusercontent.com`. So a
hallucinating or prompt-injected AI can propose
`https://github.com/<any-owner>/<any-repo>/releases/download/v1/setup.exe` as the
"official installer" for *any* app and it passes the host gate — unlike the
vendor-domain path, which requires strict brand-label equality
(`host_matches_name`, `:123-137`). The remaining backstop is the Authenticode
signature policy, but at the default `RequireValid` that only requires *some* valid
signature, not the vendor's — a cheaply code-signed unrelated exe passes. This runs
in an unattended SYSTEM install pipeline.

**Fix.** For the `TRUSTED_HOSTS` (multi-tenant release-host) path, additionally
require the URL path's owner/`owner/repo` segment to correlate with the app — reuse
the existing `alnum_token`/brand-token comparison against the app name, or match it
against the AI-declared official repo/`releases_url` if the plan carries one.
Because a strict owner-token match could reject legitimately-named forks/mirrors,
the pragmatic alternative (or complement) is to **default
`native_signature_policy` to `RequirePublisherMatch` whenever the host is a
multi-tenant release host** rather than the vendor's own domain. Pick one; document
the trade-off in the code comment. (Vendor-domain downloads are unaffected — they
already require brand equality.)

**Verify.** Unit test: `github.com/microsoft/vscode/...` for app "Visual Studio
Code" is accepted; `github.com/attacker/evil/...` for "Krita" is rejected (or
downgraded to require publisher match). Reasoning-verified for the live install
path.

### C5 — One transient ACL failure disables *all* native installs for the life of the process

**What.** `ensure_root` locks the staging dir down to SYSTEM+Administrators via
`icacls`, run through a `std::sync::Once` (`eir-svc/src/updater/download.rs:38-39,
61-82`). If `lock_down_acl` fails the **first** time (transient disk contention, AV
interception of `icacls.exe`, the path briefly locked), `ACL_OK` latches `false`
for the rest of the process lifetime (days/weeks between restarts). Every
subsequent native install, for every app, on every cycle, then fails
`ensure_root()` with "staging directory could not be locked…", and only the very
first failure was ever logged. Worse: that error string matches no pattern in
`classify_error`, so it's `ErrorCategory::Unknown` (non-terminal) — every cycle
burns a full attempt/diagnose loop repeating the identical failure with no recovery
short of a restart.

**Fix.** Don't latch failure. Drop the `Once`; in `ensure_root`, if `ACL_OK` is
false, re-attempt `lock_down_acl` and store the result, proceeding only when it
returns true. Success stays a cheap atomic fast-path afterwards; a transient
failure self-heals on the next cycle. (`icacls` is idempotent, so a rare
double-attempt race across threads is harmless.)

**Verify.** Reasoning-verified (the failure requires a live `icacls` fault to
exercise). Keep the existing fail-closed behaviour when a *current* attempt fails.

### C6 — `FileDelete` executor has no path canonicalisation or protected-dir recheck (unlike `LogCleanup`)

**What.** The `FileDelete` arm (`eir-svc/src/executor/mod.rs:130-140`) builds a
`Remove-Item -LiteralPath '<path>'` script with only a directory/exists guard — **no
`canonicalize`, no protected-dir check**. Its sibling `LogCleanup` was hardened in
v0.23.1 to canonicalise its root and refuse protected dirs precisely against 8.3
short-names/junctions (`eir-svc/src/executor/logs.rs`). `FileDelete` is always
approval-gated (never whitelisted) and the policy gate's `is_within`
(`policy/mod.rs`) is **lexical** — it lowercases/splits components and never
resolves 8.3 aliases or junctions. So an AI-proposed
`FileDelete { path: "C:\\PROGRA~1\\...\\SYSTEM~1\\x.dll" }` passes the lexical
blocklist, the approval card shows the raw *unresolved* short-name
(`explain.rs` renders `path` verbatim), and on approval `Remove-Item` deletes the
file at its real resolved location — potentially inside `C:\Windows\System32`. Both
gates (lexical policy, human approval) are defeated by the disguised path.

**Fix.** In the `FileDelete` arm, `std::fs::canonicalize` the path first and
re-check the canonical form against the same protected-directory logic `logs.rs`
uses (reuse/lift `logs::is_protected_file` or the protected-dir list); refuse if it
resolves into a protected dir. Consider surfacing the canonical path in the
approval detail so the human approves what will actually be deleted.

**Verify.** Unit test the guard against a protected canonical target. The
short-name→System32 resolution is Windows-specific; the guard logic is
unit-testable with a temp junction or by testing the protected-dir check directly.

### C7 — Three high-frequency audit tables grow unbounded — no retention at all

**What.** Only `execution_feedback` and `approval_rejections` are pruned
(`eir-svc/src/main.rs:2283-2289`, `RETENTION_DAYS = 90`). `decisions`
(`audit.rs:44-72`), `system_state_history` (a full serialized `SystemState` JSON
blob per cycle), and `execution_log` (`audit.rs:609-628`) have **no prune/VACUUM
anywhere** (grep-confirmed). At the default 10-min cadence that's ~52k decision
rows/year plus a matching `system_state_history` row plus N execution rows, forever.
This contradicts the pattern the code itself documents for the two tables that *are*
pruned, and it eventually bloats the SQLite file and slows every windowed read
(`metric_history`, `digest_stats`, `get_recent_decisions`, the trend detector).

**Fix.** Add `prune_old_decisions` / `prune_old_metrics` / `prune_old_executions`
(`DELETE ... WHERE <timestamp-col> < ?`, same shape as the existing
`feedback::prune_old`/`learn::prune_old_rejections`) and call them alongside the
existing prune block at `main.rs:2283-2289`, same 90-day window. (`system_state_history`
is read by the dashboard timeline over a bounded window, so 90 days is safe;
confirm the timeline's max range doesn't exceed it — if it does, use that range as
the floor.)

**Verify.** Unit test each prune deletes rows older than the cutoff and keeps
newer. Existing `feedback::prune_old` test is the template.

### C8 — Advisor daily spend/escalation counters serve stale (yesterday's) values across a midnight-straddling analysis

**What.** The day-rollover reset for the advisor counters lives in the per-cycle
body (`eir-svc/src/main.rs:2392-2397`), which is only reached **after** the
`if analysis_running { … continue; }` bail (`:2263-2271`). An off-loop analysis can
run up to `ANALYSIS_MAX` (10 min). If one starts at 23:5x UTC and finishes after
midnight, every tick during it bails before the rollover runs, so
`st.advisor_spend_date` never advances. When the task completes, the
`analysis_done_rx` arm **unconditionally** writes back the task-computed
`advisor_spent_today`/`advisor_escalations_today` (`:1335-1336`) — values derived
from the pre-midnight baseline captured at spawn (`:2402-2403`). So on the new day
the counters hold yesterday's near-budget spend until the *next* regular tick (up
to `decision_interval_secs`, default 600 s) finally rolls them over. During that
window a legitimate new-day escalation can be wrongly blocked by `should_escalate`'s
budget/count check.

**Fix.** In the `analysis_done_rx` arm, before folding in the task's spend/
escalation numbers, re-check the UTC day: if `chrono::Utc::now().format("%Y-%m-%d")`
differs from `st.advisor_spend_date`, roll over first (reset counters + date) and
then add only the escalation's incremental cost, rather than restoring the stale
baseline. (Small, localised; the per-cycle rollover stays as the primary path.)

**Verify.** Reasoning-verified (needs a clock straddle to exercise live). A unit
test around the fold-in logic is worthwhile if it can be factored out of the arm.

### C9 — `LogCleanup` has no internal deadline; it keeps deleting after being reported "abandoned"

**What.** `logs::cleanup` walks with `walkdir::WalkDir::new(dir)` and no time bound
(`eir-svc/src/executor/logs.rs:79`), unlike `disk_scan::scan`, which threads a
`deadline: Instant` checked each iteration (`disk_scan.rs`). If the walk runs past
the executor's `exec_max` (10 min) — a very large or slow/network-mounted tree —
`spawn_executor`'s backstop `handle.abort()` reports
`"execution exceeded 10m and was abandoned"` to the UI/audit, but `abort()` only
cancels at the awaiting task's next `.await`; the `spawn_blocking` closure doing the
file-deleting walk is **not interruptible** and keeps running to completion,
deleting files with no further audit entry and pinning a blocking-pool thread.

**Fix.** Give `logs::cleanup` the same `deadline: Instant` parameter pattern as
`disk_scan::scan`, checked once per `WalkDir` iteration, so it self-terminates
instead of relying on an abort that can't reach it. Thread the deadline from the
executor call site (same budget the backstop uses).

**Verify.** Unit test the deadline short-circuits the walk. (Fold the C19
canonical-path change into the same edit — see below.)

### C10 — Advisor settings: an explicit `0` is silently coerced away on save (threshold → 60%, budget → *uncapped*)

**What.** Two `||`-on-falsy-zero bugs in `saveAdvisorSettings`/`fillAdvisorSettings`
(`ui/main.js`):

- **Low-confidence threshold.** `fill` does
  `Math.round((s.low_confidence_threshold || 0.6) * 100)` (`:1103`) and `save` does
  `(parseInt(...) || 60) / 100` (`:1113`). The server clamps this to `[0.0, 0.95]`
  (`config.rs:67`), so `0.0` is a legitimate stored value (the input even has
  `min="0"`). A saved `0` displays as `60` on reopen and, if re-saved, is
  permanently overwritten with `0.6` — the user's "never escalate on low
  confidence" choice is silently lost.
- **Daily budget.** `save` does `parseFloat(...) || 0` (`:1114`). Server-side,
  `should_escalate`'s guard is `if cfg.budget_usd_per_day > 0.0 && spent_today >= …`
  — so a `0` budget means **no cap**, not "block all escalation". Clearing the
  budget field (or a non-numeric entry) silently *disables* the cost guardrail,
  the opposite of intent (the fill path already correctly uses a `!= null` guard at
  `:1104-1105`; only save is wrong).

**Fix.** Distinguish "blank field" from "explicit 0" on both fill and save. For the
threshold, mirror the adjacent budget-fill `!= null` idiom (accept a literal `0`;
fall back to the default only when the field is empty/NaN). For the budget save,
fall back to the default `0.5` when the field is blank, accepting a literal `0`
only when typed — **and** decide the server semantics of `0`: either keep "0 = no
cap" and never let a cleared field produce it, or change the guard to treat `0` as
"block all escalation" (`>= 0.0` with the intended meaning). Recommend the former
(smaller blast radius) unless a true "zero budget = off" is wanted.

**Verify.** Reasoning-verified (frontend). Manual: set threshold 0%, save, reopen —
stays 0%; clear budget, save — falls back to default, not uncapped.

### C11 — Tray "Pause Monitoring" bypasses the `ensure_connected` gate that B1 added everywhere else

**What.** B1 (v0.24.1) gated every pipe-facing **Tauri command** behind
`ensure_connected`, but the **tray menu** "Pause Monitoring" handler sends
`UiMsg::TogglePause` straight to the raw `mpsc::Sender`
(`eir-ui/src/main.rs:658-663`), never checking the connected flag. When the service
is down/restarting (and the tray may be the only surface if the window is closed),
the click silently queues with **no error surface** (menu items can't show
"Failed:…"). The client only drains `cmd_rx` at the moment a connection *drops*, so
a command queued while already disconnected can be **replayed on the next
reconnect** as a stale toggle against the user's current intent — silently
pausing/unpausing the guardian.

**Fix.** Gate the tray pause handler with the same `ConnState`/`ensure_connected`
check the commands use (the closure already has `app.state::<ConnState>()` access);
when disconnected, drop the send and optionally raise a native notification. (Audit
the other tray handlers — "Open Status"/"Quit" are UI-local and fine; only the
pipe-sending one needs the gate.)

**Verify.** Compile-verified; the disconnected replay is live-only — flag it.

### C12 — `match_installed` picks an arbitrary app on an ambiguous name match (wrong version baseline)

**What.** `match_installed` (`eir-svc/src/updater/names.rs:159-179`) returns the
longest-key containment match with no check that ambiguous matches agree —
`max_by_key(|(k,_)| k.len())`. Its sibling `winget_installed_version_by_name`
(`verify.rs:100-120`) deliberately returns `None` unless **all** containment
matches share the same version. So for a generically-named AI-reported app (e.g.
"Studio" matching both "OBS Studio" and "Visual Studio"), `native_candidates_from`
(`check.rs:312`) picks whichever display string is longer and uses *its* version as
`candidate.current` — feeding a wrong "currently installed" baseline into
`is_newer` and the install-plan prompt, so an update can be spuriously offered or
wrongly withheld. Bounded (can't bypass the host/signature gates), but it
mis-times/misinforms installs.

**Fix.** Mirror `verify.rs`'s discipline: collect all containment matches and return
`Some` only when they agree on one distinct version, else `None`.

**Verify.** Unit test with an installed map containing two apps that both
contiguously contain a token → `None`; a single unambiguous match → its version.

---

## P3 — papercuts, defence-in-depth, cosmetic

### C13 — The updater's per-cycle cap has no fairness rotation, so a stuck front-of-list app can starve the tail

**What.** `orchestrator.rs:349-352` does
`check.candidates.into_iter().take(max_apps_per_run)` and `check.rs:244-248` does
`apps.truncate(AI_CHECK_CAP)` (20). Candidates are rebuilt in a **fixed order**
(winget→choco→scoop→msstore→native) each cycle. In the normal case a successful
install drops an app out of `candidates` next cycle so the tail advances — but an
app that *persistently fails* at the front (a broken package, or every native
install failing via C5) stays there and permanently blocks apps past position 20
from ever being attempted. The AI-check cap has the same shape for lookups.

**Fix.** Order candidates by staleness before truncating — track "last attempted
at" (the `update_attempts` history already persists per-app timestamps) and prefer
least-recently-attempted, so the cap becomes a fair rotating window rather than a
permanent top-N. One remedy covers both call sites. If that's too much for this
patch, at minimum `log()`/note the apps dropped past the cap so the truncation
isn't silent.

**Verify.** Unit test the ordering (given N>cap candidates with timestamps, the
oldest are selected). 

### C14 — A tick during an in-flight analysis cancels a pending reactive reaction, deferring the fast-path to the next full tick

**What.** The per-cycle body unconditionally sets `react_at = None` (and resets
`last_cycle_at`) at `eir-svc/src/main.rs:2108-2112`, *before* the
`if analysis_running { … continue; }` bail at `:2263`. So if a `ticker.tick()`
fires while an analysis is running (up to 10 min), it clears the debounced reaction
scheduled by a collector's `trigger_rx`, then bails without analysing — the
signals aren't lost (`last_fingerprint` is unchanged), but the promised ~10–70 s
reactive window is defeated and the reaction waits for the next scheduled tick (up
to `decision_interval_secs`, default 600 s).

**Fix.** Preserve the pending reaction across an `analysis_running` no-op — don't
clear `react_at` (or push `last_cycle_at`) when the cycle is going to bail.
**Caution:** a naïve reorder that leaves `react_at` in the past will busy-loop the
`sleep_until(react_at)` arm (it fires immediately, re-bails, re-fires). The clean
approach is to gate the reactive `sleep_until` arm on `!analysis_running` (so it
stays dormant while an analysis runs and fires once the analysis completes, since
`trigger_rx` still accumulates `react_at` during the run), or on bail reschedule
`react_at` to shortly after the analysis is expected to finish. Let the implementer
pick; the busy-loop is the trap to avoid.

**Verify.** Reasoning-verified; the timing race is live-only. Add a comment
documenting the invariant.

### C15 — `SetAppIgnore` lets any local user grow `config.toml` unboundedly and hammer disk

**What.** The pipe is writable by any Authenticated User. `UiMsg::SetAppIgnore`
(`eir-svc/src/main.rs:1756-1787`) pushes `id.to_lowercase()` into
`cfg.updater.ignored` (exact-match dedupe only) and inserts `note` into a map, then
**unconditionally `config::save(&cfg, "config.toml")`** on every message. A local
process can stream distinct random ids to grow the ignore-list/notes without bound
and hammer synchronous disk writes on the LocalSystem config — persisted across
restarts. Impact is disk bloat / I/O amplification (not data loss or wrong
execution) on a single-user machine, hence P3.

**Fix.** Cap `cfg.updater.ignored`/`notes` at a sane bound (a few hundred, well
above real installed-app counts) and reject inserts past it; optionally validate
`id` against the last-known `st.updater.apps` before accepting, mirroring the
server-reconstructs-from-scan-state pattern used by `CleanDiskEntry`/`SetStartupEntry`.

**Verify.** Unit test the cap. 

### C16 — `Problem.confidence` is deserialized from AI JSON with no `[0,1]` clamp

**What.** `Problem.confidence: f32` (`eir-svc/src/models.rs:176-184`) takes whatever
the model emits; nothing clamps it. Consumers are a `<` threshold comparison
(`policy/mod.rs`) and a penalty subtraction floored at `0.0` (`main.rs:1416-1419`),
so an out-of-range value can't unlock anything a legitimate `1.0` doesn't already —
this is **cosmetic**, not an authority-expansion bug (the UI would render "500%" or
"-30%"). Still worth a one-line defensive clamp.

**Fix.** After deserializing the decision, clamp each problem:
`p.confidence = p.confidence.clamp(0.0, 1.0);` in a loop over `decision.problems`
(single point; `ClaudeDecision`/`Problem` have no custom deserializer today).

**Verify.** Unit test the clamp.

### C17 — The learned-fact labeller's AI usage is discarded and never billed

**What.** `learn/label.rs:50-51` binds the usage tuple to `_usage` and drops it, so
the labeller's token/dollar cost never reaches `usage_log`/`usage_summary`/advisor
accounting — a gap in the "surface all AI spend" invariant the rest of the app
upholds (`digest.rs` explicitly returns `(text, usage)` "so the caller can bill
it"). Bounded impact (≤1 call per new fact, steady-state ~zero), hence P3.

**Fix.** Propagate the usage out of `label_one` (return it, or log it inline via
`audit::log_usage`) the way `digest.rs`/`main.rs:1930-1932` do.

**Verify.** Compile-verified; assert the usage is logged if a test seam exists.

### C18 — GBP cost cells freeze at the fallback exchange rate after a cold start

**What.** `gbpRate` starts at the hardcoded `0.79` and is overwritten only once the
async `gbp_per_usd` (PowerShell + HTTPS) resolves — seconds after boot, by which
time the faster local `get_status` has already driven the first `renderUsage`,
caching `usageSig` (which hashes only `{u, provider}`, **not** `gbpRate`,
`ui/main.js:767`). So the `£` cost cells stay computed off the stale 0.79 rate until
the underlying usage numbers next change. Cosmetic, self-heals.

**Fix.** Include `gbpRate` in `usageSig` so the cells recompute when the real rate
lands. (One field in the signature object.)

**Verify.** Reasoning-verified.

### C19 — `LogCleanup` root-junction TOCTOU: the walk reopens the original path, not the checked canonical one

**What.** `logs::cleanup` canonicalises `dir` and checks `root_too_broad` on the
canonical form (v0.23.1), but then walks `WalkDir::new(dir)` using the **original
path string** (`eir-svc/src/executor/logs.rs:79`), and `walkdir` always follows the
root even with `follow_links(false)` (`follow_root_links` defaults `true` and is
never overridden). So a directory the AI legitimately proposed for cleanup that a
lower-priv user can swap for a junction *after* the check but *before* the walk gets
followed to a protected target; the per-file `is_protected_file` guard checks the
`dir`-prefixed reported path, not the resolved one, so it never fires. Narrow
(requires a precisely-timed swap of a writable proposed dir) but the fix is nearly
free and closes the same bug class v0.23.1 targeted.

**Fix.** Pass the **canonicalised** path to `WalkDir::new(...)` (removing the
check→act path divergence), and/or call `.follow_root_links(false)` and treat a
root reparse point as a hard refusal. Bundle this into the C9 edit (same function).
Also fix the now-false comment at `:63-64` ("guarding the root is enough").

**Verify.** Unit test / reasoning-verified; the live swap race is not exercised.

### C20 — Defence-in-depth & doc drift: `RegistryReset` allowlist omits `StartupApproved`; ARCHITECTURE.md lists a removed allow entry

**What.** `registry.rs`'s `ALLOWED_KEY_PREFIXES` includes the broad
`HKCU:\SOFTWARE\Microsoft` and `DENIED_KEY_PREFIXES` enumerates Run/RunOnce/
Winlogon/IFEO/etc. but **not** `…\Explorer\StartupApproved`. `RegistryReset` is
always approval-gated, so this isn't a live bypass, but it lets the startup-toggle
"closed set" (which `startup.rs` and ARCHITECTURE.md claim is only reachable via the
validated `executor::startup` path) be reached instead via a generic, less-
scrutinised `RegistryReset` approval card. Separately, ARCHITECTURE.md's
`RegistryReset` allowlist table still lists "Session Manager", which the code
deliberately **removed** — stale doc that misrepresents what's protected.

**Fix.** Add `…\Explorer\StartupApproved` to `DENIED_KEY_PREFIXES` for consistency
with the file's stated philosophy; correct the ARCHITECTURE.md table (drop "Session
Manager"). Both trivial.

**Verify.** Unit test the deny-list entry; doc change is manual.

### C21 — Minor cleanups (fold into the same release)

- **Anthropic SSE `[DONE]` dead code** (`eir-svc/src/ai/client.rs:436-477`): the
  `data: "[DONE]"` termination check is OpenAI-only; Anthropic never sends it, so
  the branch is dead (the loop already terminates correctly on stream end). Drop it
  or comment it as a deliberate no-op so a future reader doesn't assume Anthropic
  emits it.
- **`releases_url` computed, unit-tested, then discarded** (`native.rs:112`,
  `_releases`): `plan_from_response` returns a manual-download URL that
  `make_plan` drops, so the tested "manual fallback" link never reaches the UI.
  **Decide:** either wire it through `PlanOutcome`→`AttemptOutcome.detail` (append
  "— manual download: {url}" when `plan` is `None`), or delete the now-dead return
  value and its test if the fallback isn't wanted. Don't leave it half-built.
- **Model-input placeholder not updated on provider switch** (`ui/main.js:974-978`,
  `updateProviderHint`): only `#provider-hint` updates; `#set-model`'s placeholder
  stays the static OpenRouter example. Set the placeholder per-provider in
  `updateProviderHint()`.
- **`learned-list` Pin/Disable/Forget buttons lack the double-submit
  disable/re-enable guard** the sibling controls use (`ui/main.js:955-963` vs
  `484-495`). Server op is idempotent so it's UX-only; mirror the existing pattern.
- **`status.lock()` poison-recovery inconsistency** (`eir-ui`): only `get_status`
  recovers from a poisoned mutex; the five loop sites (`main.rs:477,706`,
  `pipe_client.rs:41,47,110`) use bare `.unwrap()`. Latent (those sections are
  infallible today). Factor `get_status`'s recovery into a
  `lock_status(&SharedStatus) -> MutexGuard<StatusPayload>` helper and use it at all
  six sites.

---

## Release chore (same `[release]` commit)

- **ARCHITECTURE.md:** bump the "Release: v0.24.1"/version-in-lockstep line to
  v0.24.2; describe the executor drain-on-shutdown seam (C1); the `verify_exe`
  drive-letter guard (C2); the `strip_fences`/raw-fallback change (C3); the GitHub
  host-correlation / signature-policy posture (C4); the audit-table retention
  additions (C7); and correct the stale `RegistryReset` allowlist table (C20,
  drop "Session Manager"). Update the "Known limitations & backlog" bullets that
  these fixes resolve (unbounded `system_state_history` → now pruned; the
  fire-and-forget note re: the tray-pause gate).
- **CONTEXT.md:** session note for the v0.24.2 correctness/hardening sweep.
- **Version bump v0.24.2** everywhere + `Cargo.lock` sync, `[release]` marker, tag,
  single rolling release.

## Considered and rejected (checked, not worth a change)

- **Prompt-injection via log `content_excerpt` / feedback text flowing into the
  prompt:** a real vector but consistent with the documented threat model — AI
  output is only ever gated through `parse_fix_action` + the policy allowlist,
  never executed directly. C2/C4/C6 harden the specific sinks; the general vector is
  by-design.
- **`Problem.confidence` as a security issue:** it's cosmetic only (see C16) — the
  gate is a `<` comparison, not proportional, so out-of-range values expand no
  authority.
- **Kilo NDJSON "last `step_finish` wins" cost accounting:** matches the code's own
  comment and tests; can't be shown wrong without live Kilo CLI output.
- **WMI/event-log/service-enum raw-buffer FFI casts:** buffer-growth and
  NUL-scan bounds are correct and match existing tests; a pervasive, pre-existing
  pattern, no OOB found.
- **`MpsSvc` absent from the service blocklist:** it's `NOT_STOPPABLE` at the SCM
  level, so the OS refuses the control regardless — config completeness, not a code
  defect.
- **mpsc command channels bounded (svc 8, UI 16):** low-volume commands; a stalled
  writer applies harmless backpressure in its own task, no app-wide effect.
- **`decisions.execution_output` dead column, `improvement_score` weighting,
  coarse after-state attribution:** pre-existing design limitations already noted in
  ARCHITECTURE.md's backlog; not regressions and out of scope for a bug-fix patch.
- **Poison-recovery on the loop mutexes generally:** only reachable after an
  unrelated panic already occurred while holding a lock; C21 tidies the UI-side
  inconsistency but the service-side cascade isn't worth gold-plating.
