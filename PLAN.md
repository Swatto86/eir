# Eir — Bug-fix & feature plan (handover to implementer)

**Baseline:** v0.22.1 (`fbb0a97`), tree clean, synced with `origin/master`.
**Method:** five independent adversarial reviewers (service core, AI layer, updater,
executor/signals, UI/wire-contract), each finding refuted before inclusion. Every
item below was re-verified against source by hand — file:line anchors are current.

**Verification level of this document:** source-read / compile-reasoned only. Nothing
here was exercised in a running service. Flags marked ⚠️ touch auto-executing paths
that must be live-tested after the fix.

Ordering is by severity, then value/effort. Suggested sequencing is at the bottom.

---

## Bugs

### B1 — ⚠️ CRITICAL: registry allowlist uses raw string prefix, not path-component boundary
- **Where:** `eir-svc/src/executor/registry.rs:21-23` (allowlist), dead-code net at
  `eir-svc/src/policy/mod.rs:96-98` + `policy.toml` blocklist.
- **Root cause:** `lower.starts_with(&p.to_lowercase())` treats the allowlist as a raw
  string prefix. `HKCU:\SOFTWARE\MicrosoftEvil\...` passes the `HKCU:\SOFTWARE\Microsoft`
  entry because it is a *string* prefix but not a real subkey. `registry_reset` is on
  `policy.toml`'s auto-execute whitelist, so at ≥ confidence-threshold this runs with **no
  human approval**. The policy-layer backstop is dead code: `RegistryReset { .. } if
  self.path_blocked(key_path)` calls `path_blocked`, which normalises *filesystem* paths and
  whose blocklist is all `C:\...` entries — it can never match an `HKLM:\`/`HKCU:\` string.
  So `registry.rs`'s allowlist is the only real gate, and it has exactly the boundary bug
  that `policy::path_blocked` was already hardened against (see its `normalize_path_lexical`
  + regression test `path_blocklist_resists_separator_and_traversal_bypasses`).
- **Second, related concern:** even *without* the boundary bug, `HKCU:\SOFTWARE\Microsoft`
  is a very broad grant — it covers `…\Windows\CurrentVersion\Run` (a persistence/autostart
  location). Treat the allowlist breadth as part of this fix, not just the matcher.
- **Fix:** add a `registry_key_allowed(key)` that splits both key and each prefix on `\`
  and matches component-by-component (mirror `normalize_path_lexical`'s boundary logic);
  reject on any non-boundary match. Reconsider whether the four prefixes need to be as broad
  as they are (at minimum, exclude `…\CurrentVersion\Run*`). Also make the policy layer
  registry-aware (a registry blocklist, or drop the misleading dead arm).
- **Test:** unit table proving `HKCU:\SOFTWARE\MicrosoftEvil` and
  `…\Services\TcpipXYZ` are **rejected** while genuine subkeys pass — analogue of the
  existing filesystem regression test.

### B2 — ⚠️ HIGH: CLI-provider stdin write can deadlock before output is drained
- **Where:** `eir-svc/src/ai/client.rs:619-624` (`call_claude_cli`) and `723-729`
  (`call_kilo_cli`).
- **Root cause:** `child.stdin.write_all(prompt).await` is awaited to completion *before*
  `wait_capped` begins draining stdout/stderr. If the prompt exceeds the OS pipe buffer
  (~64 KB on Windows) and the Node/Bun CLI writes to stdout/stderr before fully consuming
  stdin, both sides block: the CLI on a full stdout pipe, Eir on a full stdin pipe. A busy
  machine's prompt (multiple log excerpts up to 2500 chars each + snapshot JSON + history)
  can exceed 64 KB. On the analysis path the 10-min `ANALYSIS_MAX` task-abort is the only
  backstop; on the labelling path there is none (see B3).
- **Fix:** drain concurrently. Take stdout/stderr readers before/at spawn and run the stdin
  write concurrently with reading (`tokio::join!` the writer future with the capped
  read/wait, or spawn the writer as its own task and drop stdin on completion). This removes
  the deadlock on both CLI providers and is the root-cause fix for B3's hang vector.
- **Test:** hard to unit-test without a real CLI; add a focused test with a stub child that
  emits > 64 KB to stdout before reading stdin, asserting the call completes rather than
  hangs. If a stub is impractical, document as compile-verified and live-test with a large
  synthetic prompt.

### B3 — HIGH: labelling task has no timeout, so a hang latches the labeller off forever
- **Where:** `eir-svc/src/main.rs:1523-1542`.
- **Root cause:** the Tier-2 labeller is a bare fire-and-forget `tokio::spawn` guarded only
  by a `ResetOnDrop` that clears the `labelling` AtomicBool on completion/panic. A task that
  hangs forever (e.g. via B2 on a CLI provider) never completes and never drops, so the flag
  stays `true` and the labeller is disabled for the rest of the process lifetime. Unlike the
  analysis task (`ANALYSIS_MAX` + `inner.abort()`, main.rs:1667) there is no `timeout`.
- **Fix:** wrap `label_one` the same way the analysis task is wrapped — inner
  `tokio::spawn` under `tokio::time::timeout`, aborting on elapse so the flag is released.
  Cheap; keep it even after B2, as defence-in-depth.
- **Test:** none practical at unit level; covered structurally by matching the analysis
  task's proven pattern.

### B4 — ⚠️ MEDIUM: LogCleanup recurses past the policy-checked root into blocklisted dirs
- **Where:** `eir-svc/src/executor/logs.rs:22-52`; policy gate at
  `eir-svc/src/policy/mod.rs:93-95`.
- **Root cause:** policy `path_blocked` only checks the scan-root `path`. `logs::cleanup`
  then `WalkDir`s recursively and deletes any file with a cleanable extension
  (`log/tmp/dmp/etl/blf/regtrans-ms`) older than `days_old`, with no per-file blocklist
  recheck. `LogCleanup { path: "C:\\", days_old: 0 }` passes policy (bare root not
  blocklisted) and then recurses into `System32` etc., deleting `.etl`/`.regtrans-ms`/`.log`
  files that legitimately live there. `log_cleanup` is on the auto-execute whitelist.
- **Fix:** re-apply the blocklist per discovered file inside `cleanup` (skip any file whose
  normalised path is under a blocklisted directory, reusing the same component-boundary
  matcher). Additionally constrain the scan root to an allowlist of known log locations, and
  refuse a bare drive root / `days_old == 0`. Belt and suspenders: policy should reject a
  root that is an *ancestor* of a blocklisted dir, not just one that equals/descends it.
- **Test:** unit test that `cleanup` given a root containing a blocklisted subdir does not
  delete files under that subdir.

### B5 — MEDIUM: audit DB opened without WAL / busy_timeout; write errors swallowed
- **Where:** `eir-svc/src/audit.rs:12-19` (`init_db`); all writer fns log-and-drop on error.
- **Root cause:** pool opens with `?mode=rwc` + `create_if_missing` only — no
  `journal_mode(Wal)`, no explicit `busy_timeout`. Multiple concurrent writers by design
  (decision loop, executor worker, update-cycle task, labeller) serialize on the default
  rollback journal. sqlx's 5 s default busy-timeout makes routine contention survivable, but
  any contention exceeding it fails, and every writer just `warn!`s and continues — so a lost
  write silently drops audit history, breaks the rate-limit circuit breaker for that action
  (its `execution_log` row never lands), and NULLs effectiveness feedback.
- **Fix:** in `init_db`, `.journal_mode(SqliteJournalMode::Wal).busy_timeout(Duration::…)`
  (one line, high value; WAL lets readers and one writer proceed concurrently). Optionally add
  a small bounded retry on `SQLITE_BUSY` for the writers that feed the rate-limiter.
- **Test:** existing suite; assert WAL is set via a `PRAGMA journal_mode` read-back if easy.

### B6 — MEDIUM: Choco `Force` remedy is validated and dispatched but never applied
- **Where:** `eir-svc/src/updater/methods/choco.rs:82-107`; dispatch in
  `eir-svc/src/updater/orchestrator.rs` (~69-73), `domain.rs` `supports_force`/`has_manager_lock`.
- **Root cause:** the AI diagnostician can propose `Retry { method: Choco, remedy: Force }`;
  the validator accepts it (Choco is in `supports_force`) and dispatch computes `force = true`,
  but `choco::attempt(candidate)` takes no force parameter and its args are the fixed
  `upgrade <pkg> -y --no-progress --no-color` — `-f`/`--force` is never appended (`-y` only
  auto-confirms prompts; it does not force a reinstall). The retry re-runs the identical
  failed command and burns one of `max_attempts_per_app` (default 3). Same bug class the code
  says it already fixed for `ClearManagerLock` — but only wired through to winget.
- **Fix:** thread `force: bool` into `choco::attempt` and append `--force` when set.
- **Test:** unit test on the arg builder asserting `--force` present iff force requested.

### B7 — LOW/MEDIUM: "Ignore app" gives feedback that the next poll clobbers
- **Where:** `eir-svc/src/main.rs:1353-1373` (`SetAppIgnore`); `ui/main.js:465-468`.
- **Root cause:** the handler persists `cfg.updater.ignored` but never updates
  `st.updater.apps` (only rewritten on update-cycle completion, main.rs:850, or clear:1323).
  The immediate `broadcast_status(build_status(&st))` carries the unchanged apps list, and
  `renderUpdater` rebuilds `#updater-apps` innerHTML wholesale every 2 s poll, so the JS's
  optimistic `opacity:.5` is wiped and the ignore looks like a no-op until the next cycle.
  The ignore itself is not lost — it takes effect next cycle — only the feedback is.
- **Fix:** in the handler, set an `ignored`/`skipped` marker on the matching
  `st.updater.apps` row before broadcasting (add a field to `UpdaterAppRow` in `eir-proto`
  if none fits), and have the UI render ignored rows from that. Keep `#[serde(default)]` for
  wire-compat.
- **Test:** none critical; visual.

### Low-severity notes (fix opportunistically; not release-blocking)
- **L1** `updater/verify.rs:125-142` — AI-supplied `verify_exe` is only checked
  `is_absolute`, not tied to the app's install dir; a wrong path whose version coincidentally
  matches could yield a false "Verified" on the fallback path. Constrain under Program
  Files / the app's install dir. Read-only, low impact.
- **L2** `main.rs:491-509` — `EXEC_MAX` abort also cancels the trailing audit/feedback
  writes; a > 10-min DB stall after a *successful* fix would lose its `execution_log` row and
  let the rate-limiter re-run it. Very low probability; partly mitigated by B5 (WAL). Consider
  running the audit writes outside the aborted region.
- **L3** `signals/event_log.rs:141-153` — source-string NUL scan has no buffer bound; a
  malformed record could walk past the buffer (needs a high-privilege local actor to inject).
  One-line fix: cap `len` at remaining buffer.
- **L4** `main.rs` `push_problem` vs `policy::evaluate` — a `Block` reason embeds the
  learned-penalty-adjusted confidence % while the card shows the raw pre-penalty %; the two
  can disagree. Display only. Show the same number in both.
- **L5** `executor/mod.rs` `FileDelete` — no reparse-point/symlink guard; latent only
  because `file_delete` is not whitelisted. Add the guard before ever whitelisting it.
- **L6** `ai/client.rs:683` — Kilo CLI workspace is keyed by process PID only, so a
  concurrent analysis + labeller Kilo call share one `--dir`; also never cleaned up (litters
  `%TEMP%`). Use a per-call unique dir and remove it after.
- **L7** `ai/client.rs` SSE paths — a mid-stream truncation surfaces as "Failed to parse
  model response as JSON" rather than "stream truncated". Diagnostics only.
- **L8** `ui/main.js` `renderLearned` / parts of `renderUpdater` — unconditional innerHTML
  rebuild every 2 s wipes text selection (the Approvals list already got a signature-diff
  guard; siblings didn't). Cosmetic.

---

## Features (ranked by value/effort)

F1–F6 are grounded in the project's own backlog (`ARCHITECTURE.md` "Known limitations",
`CONTEXT.md` open questions); F7–F9 are net-new capability beyond the backlog. Recommend
F1+F2+F3 first — small and independent; the rest are worthwhile but larger.

### F1 — Anthropic prompt caching (recommended)
The ~110-line static guardrail prose is re-sent uncached every cycle on the Anthropic native
path (`ARCHITECTURE.md` backlog). Add `cache_control` breakpoints on the static system
prompt in `ai/client.rs`. Direct, ongoing cost reduction; small, self-contained change.

### F2 — Persist advisor day-counters (recommended; safety-cap correctness)
`advisor_spent_today` / `advisor_escalations_today` are in-memory and reset on restart, so a
service restart resets the daily spend/escalation ceiling — a real bypass of a safety cap
(`ARCHITECTURE.md` backlog). Persist them to the audit DB keyed by the UTC date they belong
to; reload on startup and reconcile on date flip. Small; arguably a bug/feature hybrid.

### F3 — Automated version-sync check in CI (recommended; cheap footgun guard)
No check verifies the four version locations stay in sync (3× `Cargo.toml` +
`eir-ui/tauri.conf.json`); drift ships silently (`CONTEXT.md` open question, backlog). Add a
~15-line script step in `ci.yml` that fails if they disagree. Cheap; prevents release
mistakes. While here, resolve the stale root `tauri.conf.json` (remove or make it an explicit
shim) — the other standing `CONTEXT.md` open question.

### F4 — Registry undo snapshot (pairs with B1; improves the trust story)
`RegistryReset` is marked `reversible=false` because the prior value is never captured
(`explain.rs`, backlog). Snapshot the existing value before `reset_value`, persist it, mark
the action reversible, and expose a one-click undo in the UI. Turns a scary auto-executing
action into a reversible one — meaningfully better safety posture. Medium effort; natural to
land alongside the B1 registry work.

### F5 — Native notification on new pending approval
An unattended guardian currently surfaces approvals only if the user opens the window. Fire an
OS notification (Tauri notification plugin) when a fix needs approval, deep-linking to the
Approvals view. Closes the loop for the tray-resident use case. Small–medium.

### F6 — Put `system_state_history` to use (larger; propose, don't commit yet)
Rich per-cycle metrics are written every cycle but **never read** (backlog). A lightweight
trend signal — "CPU climbing N cycles", "disk trending toward full" fed into the AI prompt
and/or a UI sparkline — would turn dead data into signal. Bigger scope and needs a design
pass on what trends are actionable; recommend deferring to its own cycle rather than bundling.

### F7 — SMART disk-health signal (net-new)
`SystemState.disk_health` is hardcoded `"unknown"` and `network_errors` hardcoded `0`
(`models.rs`, backlog "neither is actually measured"). Collect real SMART status via
`Get-PhysicalDisk`/`Get-StorageReliabilityCounter` (or WMI `MSStorageDriver_FailurePredictStatus`)
in the existing wmi collector cadence, bounded like the other probes (`ps_capped`). Feed it
into `actionable_fingerprint` so a disk predicting failure triggers a reactive analysis —
a failing disk is exactly the early warning a guardian exists for. While in there, wire
`network_errors` from `GetIfEntry2` error counters or drop the field. Medium effort; new
signal only, no new actions, so no policy surface change.

### F8 — DISM/SFC system-file repair actions (net-new; revisit of a deliberate deferral)
`sfc /scannow` and `DISM /Online /Cleanup-Image /RestoreHealth` were deferred when execution
ran inline because they block for many minutes (see `eir-breadth-theme` memory / backlog).
That blocker is gone: fixes now run on the off-loop executor worker with `EXEC_MAX` = 10 min.
Add two `FixAction` variants (`SfcScan`, `DismRestoreHealth`) — **require-approval, never
whitelisted** (long-running, writes to the component store), with a dedicated generous
timeout (SFC can exceed 10 min; give these their own cap rather than EXEC_MAX), progress
surfaced via the existing execution feed, and prompt guidance so the AI proposes them only
for corruption-signature events (CBS/ESENT/WHEA errors). Medium–large; the highest-leverage
"fix everything" breadth item.

### F9 — Weekly plain-English health digest (net-new)
The audit DB holds decisions, executions, feedback scores, update attempts, and (with F6)
metric history — but there is no retrospective view. Once a week, generate a short digest —
what Eir saw, fixed, blocked, learned, spent — as one bounded AI call over aggregated audit
rows (counts/summaries, not raw snapshots), surfaced as a new UI card and an OS notification
(reuses F5's plumbing). Costs one cheap call a week; makes the guardian's value visible
instead of silent. Medium effort; natural after F5, benefits from F6.

---

## Suggested sequencing for implementation

1. **Safety-critical bug pass (one release):** B1, B4 (both auto-executing path bypasses),
   B2 + B3 (CLI deadlock + labeller latch), B5 (WAL). Adversarial-sweep + `--all-targets`
   clippy + `cargo test --workspace`, then live-test the ⚠️ auto-exec paths (registry reset
   against a throwaway key; log cleanup against a scratch tree) before tagging.
2. **Correctness/UX bug pass:** B6, B7, and the L-notes worth taking (L3, L4, L6, L8).
3. **Feature cycle:** F1 + F2 + F3 together (all small, independent), then F4 alongside the
   B1 registry work if not already merged. F5 next.
4. **Signal & breadth cycle (each its own release):** F7 (SMART/network signals), F8
   (DISM/SFC actions — approval-gated, own timeout, live-test one run before tagging),
   F6 (trend design pass), then F9 (digest — last, so it has F5's notification plumbing
   and F6/F7's richer data to report on).

Per repo policy: bump all four version locations + `Cargo.lock` together, `[release]` marker,
CI green is the only pre-release gate, single rolling release. Live-run verification of any
auto-executing fix is required *before* tagging for pass 1 specifically, given the blast
radius of B1/B4.
