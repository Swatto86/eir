# Eir — Game Mode (game-aware quiet mode) (handover to Opus)

**Baseline:** v0.25.4 (`adddf3c` + the test-harness commit `9f6137e`), tree clean.
**What this is:** a "Game Mode" that makes Eir *get out of the way* while a fullscreen
game is running — it suppresses Eir's own disruptive background activity (updater cycles,
housekeeping, the weekly digest) and, optionally, switches to a high-performance power plan
— then restores everything when the game exits.

**What this is deliberately NOT:** it does **not** kill "unnecessary" apps or stop services
for FPS. That idea was assessed and rejected: the FPS benefit is mostly placebo on modern
hardware (Windows already ships Game Mode; stopping idle services yields ~zero measurable
gain), "unnecessary" is undecidable and breaks real setups (Discord-for-voice, VPNs,
Defender real-time = a security hole), it fights Eir's own guardian loop (stopped services
read as faults), and the batch-restore is a crash-unsafe transaction. This design keeps the
*intent* (Eir stops interrupting your game) while making it safe: the only system change it
makes is the optional power-plan switch, whose "restore" is a single value, not a batch.

---

## Design decisions (read before implementing)

**Detection runs in the tray, not the service.** `EirSvc` runs in session 0 (LocalSystem)
and cannot see the interactive user's fullscreen state. The tray app (`eir-ui`) runs in the
user's session and can. So the tray detects "a fullscreen game is running" and reports it to
the service over the existing pipe. The service owns the *behaviour* (what to suppress) and
the *state* (the `gaming` flag it broadcasts back).

**Detection primitive:** `SHQueryUserNotificationState` (shell32) — the same API Windows uses
to decide whether to show toasts. It returns `QUNS_RUNNING_D3D_FULL_SCREEN` (a Direct3D
exclusive-fullscreen app = a game) and `QUNS_BUSY` (a fullscreen app generally). Treat either
as "gaming/fullscreen → quiet." This is a single Win32 call, no games-list to maintain and no
window-rect heuristics. **Known limitation to document:** exclusive-fullscreen and most
borderless-fullscreen games are detected; a *windowed* game is not (that's what the manual
toggle is for). Multi-monitor: the state is global (fullscreen on any monitor triggers it).

**Stuck-flag safety (important):** the `gaming` flag is driven entirely by the tray. If the
tray crashes/closes while gaming is on, the service must not stay quiet forever. Solution: the
flag is a **lease**, not a latch. The tray re-asserts `SetGaming(true)` on a ~30 s heartbeat
while it believes a game is running; the service stores `gaming_until = now + 90 s` and treats
gaming as active only while `now < gaming_until`. An explicit `SetGaming(false)` (game exited,
or manual toggle off) clears it immediately. So a dead tray auto-expires gaming within 90 s and
Eir resumes full guardian duty. No new pipe-disconnect plumbing needed.

**Debounce the exit, not the entry.** Enter quiet mode immediately on fullscreen; on leaving
fullscreen, wait ~60 s of sustained non-fullscreen before sending `SetGaming(false)`, so a
quick alt-tab to Discord doesn't kick off an updater cycle. (Tray-side.)

**Two phases.** Phase 1 (quiet mode) makes **no system changes** — it's purely "don't do
things," so it's dead safe and independently shippable. Phase 2 (power-plan toggle) is the one
part that changes system state and needs crash-safe restore; it's opt-in and off by default.
Ship Phase 1 first; Phase 2 can follow in the same or a later release.

---

## Ground rules

- Wire compat: new `UiMsg` variant (`SetGaming`) and new `StatusPayload`/`UiSettings` fields
  must be `#[serde(default)]`; an old service logs-and-skips an unknown `UiMsg`, an old UI
  ignores unknown fields — the skew invariant holds.
- The tray is the only new user-session native code. Adding the `windows` crate to `eir-ui`
  (feature `Win32_UI_Shell` for `SHQueryUserNotificationState`, plus `Win32_Foundation`) is
  justified — it's the correct Win32 API, and `eir-svc` already uses `windows` 0.58.
- Gate before release: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace`, full tauri build via CI. Update `ARCHITECTURE.md` in the same
  commit (new decision-loop state, the `SetGaming` command, the tray detector, and — Phase 2 —
  the power-plan action + its persisted restore).

---

## Phase 1 — Quiet mode (no system changes)

### G1 — `gaming` lease state + wire field + status tier
- **Service (`eir-svc/src/main.rs`):** add to `SvcState` (struct at :193, Default at ~:291)
  a `gaming_until: i64` (unix secs; 0 = not gaming). Add a helper
  `fn is_gaming(st: &SvcState, now: i64) -> bool { st.gaming_until > now }` (pure, testable).
- **Wire (`eir-proto/src/lib.rs`):** add `#[serde(default)] pub gaming: bool` to
  `StatusPayload`. `build_status` (`main.rs:335`) sets `gaming: is_gaming(st, now)`.
- **Status tier:** `resting_status` (`main.rs:475`) gets a `Gaming` tier **below Paused,
  above PendingApproval/Executing/Active**: `if st.paused {…} else if is_gaming(st, now) {
  "Gaming" } else …`. (Paused still wins — an explicit user pause outranks auto game mode.)
  Note `resting_status` currently takes only `&st`; thread `now` in (or read
  `Utc::now().timestamp()` inside — it's already impure elsewhere). Update the
  `resting_status_precedence` test.
- **Frontend:** `STATUS_META` gets a "Game Mode" entry (headline like "Game Mode — staying out
  of the way while you play"); `status_accent`/tray tint gets a distinct colour (e.g. purple).
  Tooltip via `friendly_status` → "Game Mode".

### G2 — `SetGaming` command + handler
- **Wire:** `UiMsg::SetGaming { on: bool }` (near `RefreshStatus`, `lib.rs:472`).
- **Tauri command (`eir-ui/src/main.rs`):** `set_gaming(on: bool)` → `ensure_connected` →
  `UiMsg::SetGaming { on }`; register in `generate_handler!`.
- **Service handler (`main.rs` ui_rx):**
  - `on = true`  → `st.gaming_until = now + 90;` (extend the lease).
  - `on = false` → `st.gaming_until = 0;` **and** reset `last_fingerprint = None` so the next
    cycle re-analyses and any fault that was noticed during the game gets handled promptly
    (otherwise the idle-skip gate would sit on an unchanged fingerprint). Ping the reactive
    trigger too, so "game just ended" wakes the loop within a debounce instead of waiting for
    the next scheduled tick.
  - Either way: recompute `st.status = resting_status(...)`, broadcast.
- **Lease expiry:** in the per-tick body (and `resting_status`), gaming is derived from
  `gaming_until > now`, so it auto-expires with no extra timer. Recompute/broadcast on the tick
  where it flips off (compare previous vs current `is_gaming` and, on a false edge, reset
  `last_fingerprint`/re-settle as in the explicit-off path).

### G3 — Suppress the disruptive background work while gaming
- **Scheduled updater:** the `due` gate (the block that starts a scheduled update cycle —
  `let due = cfg.updater.enabled && !st.paused && !st.updater_running && …`) gains
  `&& !is_gaming(st, now)`. **Do NOT** gate the manual `UiMsg::RunUpdatesNow` handler
  (`main.rs:1831` area) — if the user explicitly clicks "update now" during a game, honour it.
- **Weekly digest:** the digest gate (`if !st.digest_running && (st.last_digest_at == 0 ||
  now_ts - st.last_digest_at >= DIGEST_INTERVAL_SECS)`, ~`main.rs:2357`) gains
  `&& !is_gaming(st, now)` — defer the AI digest call + its I/O until after the game.
- **Leave the reactive fix loop running** (deliberate). It only fires on an *actual fault*
  (rare during a game), the auto-whitelisted actions are non-disruptive or sub-second (a
  crashed background service getting restarted is desirable, not an interruption), and
  `process_kill`/`service_stop` are approval-gated so they never auto-fire. Deferring reactive
  fixes would need a defer/resume queue for near-zero benefit — **out of scope for v1**
  (document as a possible v2). The dominant interrupter is the updater, which G3 handles.
- **Notifications:** Windows already suppresses toasts during fullscreen (that *is*
  `QUNS_RUNNING_D3D_FULL_SCREEN`), so no core change is required. Optional nicety: gate
  Eir's own `tauri-plugin-notification` sends on `!gaming` so an approval/digest toast isn't
  spent while suppressed — low priority.

### G4 — Tray-side fullscreen detector
- **`eir-ui`:** add `windows = { version = "0.58", features = ["Win32_UI_Shell",
  "Win32_Foundation"] }`. A background task (spawned in `setup`, alongside the existing
  status-poll task) every ~5 s calls `SHQueryUserNotificationState`:
  - fullscreen (`QUNS_RUNNING_D3D_FULL_SCREEN` or `QUNS_BUSY`) → "gaming".
  - Track a small state machine: on entering gaming, send `SetGaming(true)` and re-send every
    ~30 s (heartbeat) while it persists. On leaving fullscreen, start a ~60 s debounce; if
    still non-fullscreen at the end, send `SetGaming(false)`; if fullscreen returns first,
    cancel the debounce.
  - **Only runs when the auto setting (G6) is enabled** *and* the pipe is connected
    (`ensure_connected`); when auto is off, the detector idles (the manual toggle still works).
- Keep the call cheap and off the UI thread; it's a bare Win32 call returning an enum.

### G5 — Manual toggle
- **Tray menu:** add a "Game Mode" checkable item (below Pause) that sends `set_gaming(!current)`
  — lets the user force it for a windowed/undetected game or override auto. Reflect the current
  `status.gaming` in the label ("Game Mode ✓").
- **UI:** a Game Mode toggle on the Dashboard (near Pause) mirroring the tray item.
- **Manual-vs-auto interaction (document):** if auto is on and the user manually turns Game
  Mode *off* mid-game, the detector's next heartbeat re-enables it. That's an acceptable v1
  quirk — tell the user to turn auto off if they want full manual control. (A cleaner
  manual-override-wins model is a v2 refinement, not worth the state for v1.)

### G6 — Settings
- **Config (`eir-svc/src/config.rs`):** add `#[serde(default = "…true…")] game_mode_auto: bool`
  to the monitoring/api settings struct (default **on** — it's purely "Eir does less, safely,"
  so it delivers value out of the box; the setting lets a user disable it). Surface it in
  `UiSettings` (so the tray reads whether to auto-detect) and in the `SettingsUpdate` mirror.
- **Settings UI:** a checkbox "Automatically enable Game Mode during fullscreen games" in the
  provider/monitoring card. The tray reads `lastStatus.settings.game_mode_auto` to gate G4.

### G7 — Presentation polish
- Dashboard hero shows the Game Mode state (from G1's `STATUS_META`); the tray icon/tooltip
  reflect it (G1). A one-line dashboard note ("Game Mode active — updates and housekeeping
  paused until you finish") so the behaviour is legible. All `esc()`-clean (no new untrusted
  strings — `gaming` is a bool).

---

## Phase 2 — Optional high-performance power plan (opt-in, off by default)

The one genuinely-effective lever, and the only part that changes system state. Off by default
(marginal on desktops; changes display/sleep timeouts as a side effect). Because it's a system
change, its restore must survive a crash mid-game.

### G8 — Setting
- Config `#[serde(default)] game_mode_power_boost: bool` (default **false**), surfaced in
  `UiSettings`/`SettingsUpdate`; Settings UI checkbox "Switch to High Performance power plan
  during games (restores on exit)".

### G9 — Switch + restore (service-side, LocalSystem)
- On the gaming **false→true** edge with `power_boost` on: capture the current scheme GUID via
  `powercfg /getactivescheme` (parse the GUID out of "Power Scheme GUID: <guid> (name)"),
  **persist it** (G10), then `powercfg /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c`
  (well-known High Performance GUID; `setactive` works even if the plan is hidden).
- On the gaming **true→false** edge: `powercfg /setactive <persisted-guid>`, then clear the
  persisted value. Route both through the existing timed `powershell::run_diagnostic`/proc
  helper so a wedged `powercfg` can't stall the loop.
- Idempotence: if the current scheme already equals High Performance when entering, persist it
  as the restore target too (restoring to High Performance is harmless) — or skip the switch;
  either is fine, just don't lose the pre-existing plan.

### G10 — Crash-safe restore
- Persist the saved GUID in a tiny key/value row in the audit DB (a new `app_state(key TEXT
  PRIMARY KEY, value TEXT)` table via a migration, reusable for future transient state) — set
  when entering the boost, cleared on clean restore.
- On `eir_main` startup, after `init_db`, check for a persisted `power_restore` GUID; if present
  (the service died mid-game), `powercfg /setactive <guid>` and clear it. This is the whole
  "restore even if we crashed" guarantee — one value, one startup check.

---

## Explicit non-goals (do NOT build)

- **No killing apps or stopping services for FPS** — assessed and rejected (placebo benefit,
  breaks real setups, security regression, crash-unsafe restore, fights the guardian). If a
  user really wants "close *these* apps when I game," that's a future **user-curated explicit
  list** feature, never an AI-guessed "unnecessary" set — and it's out of scope here.
- **No disabling Defender / AV / firewall for performance** — ever. Eir's security posture only
  ever *enables* these.
- **No deferring the reactive fix loop** in v1 (see G3 rationale).

---

## Tests

- `is_gaming` lease math (true while `gaming_until > now`, false at/after) — pure unit test.
- `resting_status` precedence incl. the new Gaming tier (extend `resting_status_precedence`):
  Paused > Gaming > PendingApproval > Executing > Active.
- Extract the scheduled-updater `due` predicate into a pure `fn updater_due(cfg, st, now) ->
  bool` (mirrors the `apply_live_metrics` extraction from the v0.25.4 test harness) and unit-test
  that gaming suppresses it while a manual run is unaffected — this locks G3 against regression.
- Phase 2: a pure parse test for `powercfg /getactivescheme` output → GUID (real sample +
  a malformed line → `None`/error, not a panic).
- `build_status` projects `gaming` (extend the existing projection test).
- Tray detector + the live `SHQueryUserNotificationState`/`powercfg` calls are reasoning/
  compile-verified only (no automated harness) — state so in the notes and live-exercise:
  launch a fullscreen game, confirm the hero flips to Game Mode and no updater cycle starts;
  exit, confirm it resumes; with `power_boost` on, confirm the plan switches and restores
  (including after killing `EirSvc` mid-game → restart → plan restored).

---

## Sequencing & release

1. **Phase 1 (G1–G7)** is a coherent, safe, shippable unit — do it first. One release
   (**v0.26.0**, a feature bump) once the gate is green.
2. **Phase 2 (G8–G10)** can ride the same release or a follow-up; it's independent and opt-in.
3. Version bump across the four manifests + `Cargo.lock`; update `ARCHITECTURE.md` in the same
   commit (new `gaming` lease state, `SetGaming`, the tray detector, and — if included — the
   power-plan action + `app_state` restore); commit with `[release]`; CI gate; roll the single
   release (delete prior tag/release, tag `v0.26.0`, push).
4. Live-exercise the detector + (if built) the power-plan restore before relying on them — they
   have no automated coverage.
