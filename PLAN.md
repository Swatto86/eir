# Eir — stale-status fix + manual reset (handover to Opus)

**Baseline:** v0.25.2 (`9a0e522`), tree clean, synced with `origin/master`.
**Theme:** a recovered failed-service lingers on the dashboard for up to ~10–15 min,
and there is no way to force a status refresh. Fix the root-cause latency (E1) and add
the manual "Refresh status" control the user asked for (E2). Two small, independent
changes — no new features beyond these.

---

## Root-cause analysis (verified against the source at this baseline)

`failed_services` is **not** sticky by accumulation — `get_services()`
(`eir-svc/src/signals/wmi.rs:175`) recomputes it fresh on every WMI scan, flagging only
services that are *currently* `SERVICE_STOPPED` with an abnormal exit code (exit 0 and
1077 excluded). A recovered/restarted service is `SERVICE_RUNNING` and drops out of the
set on the next scan. The set reaches the UI via
`st.failed_services = sys.failed_services.clone()` (`main.rs:2333`) → `build_status` →
broadcast. So it *does* self-clear — but on two slow clocks with a one-way wake:

1. **WMI poll cadence** — the collector re-scans only every `wmi_poll_interval_secs`
   (struct default 300 s / 5 min, `config.rs`), storing the snapshot in `wmi_shared`
   (`wmi.rs:521-555`).
2. **Wake asymmetry** — the collector pings the decision-loop trigger **only when a new,
   non-empty fault appears**: `let changed = key != last_fault_key && !key.is_empty();`
   (`wmi.rs:540`). A fault *clearing* (`key` → empty) is `!key.is_empty() == false`, so
   **it never wakes the loop**. New faults surface within ~10 s (reactive debounce);
   recoveries do not.
3. **Decision-tick cadence** — `st.failed_services` is only refreshed inside the per-tick
   body (`main.rs:2327-2334`), which runs on a `ticker` at `decision_interval_secs`
   (default 600 s / 10 min) or a reactive wake. With no recovery-wake (point 2), a
   recovered service isn't cleared until the next scheduled tick.

Combined worst case: ~5 min (WMI hasn't re-scanned yet) + up to ~10 min (next decision
tick) ≈ **up to ~15 min stale**, which reads as "doesn't clear." The dashboard render is
a pure reflection (`ui/main.js:326-331`: show the card iff `failed_services.length > 0`),
so no frontend stickiness — the fix is server-side timing plus a manual override.

**AI-spend note (important for E1):** the reactive trigger schedules an AI analysis ~10 s
later. But `actionable_fingerprint` (`main.rs:796`) includes `sys.fault_parts()`
(`models.rs:92`, built from `failed_services`). On a *pure* recovery the fault set is
empty, so the fingerprint is `None` → the idle-skip gate settles status, broadcasts, and
`continue`s **without an AI call**. So waking the loop on a recovery refreshes the UI but
does **not** burn AI spend (unless something else is independently actionable, which is
correct). E1 is verified safe against this.

---

## Ground rules

- Match repo conventions. Service owns all state; UI is a stateless renderer of the
  broadcast snapshot. The UI never constructs domain state; commands are opaque `UiMsg`s.
- **Wire compat:** adding a `UiMsg` variant is backward-safe — an old service that
  receives the new variant logs-and-skips it (`pipe_server` "bad messages are logged and
  skipped, not fatal"); a new service handles it. No `StatusPayload` shape change is
  needed (E2 reuses the existing `failed_services`/`status` fields), so the
  `#[serde(default)]` skew invariant is untouched.
- Frontend stays committed static vanilla HTML/CSS/JS — no npm, no deps. Service-supplied
  strings go through `esc()`/`escAttr()`. New command gated on connection
  (`ensure_connected` + the `svc-down` CSS class), fire-and-forget with a toast.
- Gate before release: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, full tauri build
  via CI (`scripts/check-versions.ps1` gates version sync). Adversarial multi-lens+refute
  sweep before tagging, per CLAUDE.md.
- Update `ARCHITECTURE.md` in the **same commit** (the "Signal sources" WMI-trigger
  description and the pipe/UI command list both change).

---

## E1 — Wake the decision loop when a fault *clears*, not only when one appears (root cause)

- **Where:** `eir-svc/src/signals/wmi.rs:539-547`.
- **What:** the WMI collector only triggers a decision-loop reaction on a new non-empty
  fault, so a recovery waits for the next scheduled tick (up to `decision_interval_secs`).
- **Fix:** trigger on any *change* to the fault key, including a change to empty:
  ```rust
  let key = fault_key(&s);
  // Wake the loop on ANY change to the fault set — including a fault CLEARING — so a
  // recovered service (or re-enabled firewall, etc.) is reflected within one reactive
  // debounce instead of waiting for the next scheduled decision tick. A pure recovery
  // makes `actionable_fingerprint` empty, so the idle-skip gate suppresses the AI call;
  // this refreshes the UI without spending on analysis.
  let changed = key != last_fault_key;
  last_fault_key = key;
  if let Ok(mut guard) = shared_clone.lock() {
      *guard = Some(s);
  }
  if changed {
      let _ = trigger.try_send(());
  }
  ```
  (Only the `changed` line changes — drop the `&& !key.is_empty()`.) The snapshot is
  stored in `wmi_shared` *before* the trigger fires, so even if the capacity-1 trigger is
  already full (a reaction pending), that pending reaction reads the fresh (recovered)
  snapshot from `wmi::current`. First-poll healthy state stays quiet (`"" != ""` is
  false), so no spurious startup trigger.
- **Effect:** a recovery now clears within ~1 WMI poll + ~10 s reactive debounce (≈ up to
  5 min) instead of ~15 min, with no AI-spend cost.
- **Test:** add a `wmi` unit test around a small helper if you extract the
  `changed`-decision into a pure fn (e.g. `fn fault_changed(prev: &str, cur: &str) ->
  bool { prev != cur }`) — assert it fires on appear, on clear, and on set-change, and is
  quiet on an unchanged key (incl. empty→empty). Keep it tiny; the trigger plumbing itself
  is only reasoning-verifiable (no live harness).

---

## E2 — Manual "Refresh status" command (the requested control)

Gives the user an immediate reset instead of waiting on the poll — a fast, services-only
rescan that clears a recovered service the instant they click, and re-settles the status
hero/tray. Reuses the existing broadcast, so one command updates every view that reads the
snapshot (dashboard chips, status hero, tray icon/tooltip) — the "across all windows"
requirement, given Eir's single authoritative broadcast.

**1. Wire type** — `eir-proto/src/lib.rs` (`UiMsg` enum, near `ClearProblems` at :467):
```rust
/// Force an immediate live-status refresh (fast services rescan + re-settle).
RefreshStatus,
```
No payload, no `StatusPayload` change.

**2. Fast services-only rescan** — `eir-svc/src/signals/wmi.rs` (public helper):
```rust
/// Force an immediate services-only rescan for the manual "Refresh status" command.
/// Fast (SCM enumeration, no PowerShell), so it can be awaited inline in the command
/// handler; returns the fresh failed-services set. cpu/mem/disk still refresh on the
/// normal WMI/decision cadence — the manual refresh targets the stale-service complaint.
pub async fn rescan_failed_services() -> Vec<String> {
    tokio::task::spawn_blocking(|| get_services().1)
        .await
        .unwrap_or_default()
}
```
(`get_services` returns `(running_count, failed)`; `.1` is the `Vec<String>`.) Do **not**
call the full `snapshot_state` here — `get_cpu_usage` shells out to PowerShell WMI and can
take up to 15 s (see ARCHITECTURE "Signal sources"), which would stall the `ui_rx` arm.
Services-only is sub-second.

**3. Handler** — `eir-svc/src/main.rs` (new `ui_rx` arm beside `ClearProblems` at :1687):
```rust
UiMsg::RefreshStatus => {
    // Force a fresh services scan so a recovered service clears immediately instead
    // of waiting for the next WMI poll + decision tick, then re-settle the hero/tray.
    st.failed_services = signals::wmi::rescan_failed_services().await;
    st.status = resting_status(&st);
    pipe.broadcast_status(build_status(&st));
}
```
`resting_status` (`main.rs:363`) returns Paused/PendingApproval/Executing/Active by
precedence — so once the recovered service drops out, the hero settles back to Active
(unless genuinely paused/pending/executing). This mirrors how `exec_done_rx` and the
idle-skip gate already re-settle status, so it's consistent, not a new status path.
- **Note:** awaiting the sub-second `spawn_blocking` briefly holds the `ui_rx` arm. That's
  acceptable for a manual, low-frequency click (the scan is services-only). If you prefer
  zero loop-hold, dispatch it off-loop and fold the result back via a small channel like
  `ask`/`analysis` — but that's overkill for a sub-second scan; inline is the lazy-correct
  choice here. State which you chose in the release notes.

**4. Tauri command** — `eir-ui/src/main.rs` (beside `clear_problems` at :213, and add to
the `generate_handler!` list at ~:786):
```rust
#[tauri::command]
async fn refresh_status(tx: State<'_, UiCmdTx>, conn: State<'_, ConnState>) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::RefreshStatus).map_err(|e| e.to_string())
}
```

**5. Frontend button** — `ui/index.html` + `ui/main.js`:
- Add a small "Refresh" affordance on the Dashboard status hero (or in the failed-services
  card header). Icon-only buttons need an `aria-label`/`title` (accessibility basics).
- Wire it like the other fire-and-forget actions (model on `clear-activity`,
  `main.js:928`): `await invoke('refresh_status'); toast('Status refreshed', 'ok');` with
  a `catch` → `toast('Could not refresh', 'err')`, and disable it in-flight (mirror the
  D19 Ignore-button pattern) so a double-click can't spam. It stays functional under the
  `svc-down` class only if `ensure_connected` passes — since it crosses the pipe, the
  `svc-down` CSS should grey/block it like the other pipe actions (it's not a local-only
  command).
- Toast wording: the pipe is fire-and-forget, so say "Refreshing…"/"queued", not "done"
  (consistent with the v0.25.1 toast honesty note in ARCHITECTURE).

---

## Explicit non-goals (do NOT do these — they'd cause new bugs)

- **Do not clear `st.error` in the refresh handler.** The error banner is the AI/config/
  connection error, with its own lifecycle (cleared on the next successful analysis, and
  carefully paused-guarded per D7/D14). Blanket-clearing it on a manual refresh would hide
  a *live* provider/config error. The refresh targets failed-services staleness, not the
  error banner. If an error-dismiss is ever wanted, that's a separate, explicit control.
- **Do not touch `recent_problems`/`recent_executions`** (the Activity feed). Those are
  historical and already have `clear_problems`/`clear_executions`. A "failed service X"
  entry there is a past event, not live status.
- **Do not lower `wmi_poll_interval_secs` or `decision_interval_secs` defaults** to force
  faster clearing — that raises steady-state WMI/AI load for everyone. E1 (wake-on-clear)
  + E2 (manual) address the latency without changing the cadence.

---

## Testing & verification

- **E1:** unit-test the pure change-decision helper (fires on appear/clear/change, quiet
  on unchanged incl. empty→empty). The end-to-end wake is reasoning-verified (no live
  loop harness) — say so in the notes.
- **E2:** `rescan_failed_services` is thin over the already-tested `get_services`; a unit
  test isn't load-bearing, but a compile/`cargo test` pass plus a manual live click
  (fix a service → click Refresh → chip clears) is the real check. The frontend has no
  automated harness — flag it compile/reasoning-verified.
- Gate: `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo test --workspace`,
  full tauri build via CI.

---

## Sequencing & release

1. E1 and E2 are independent; do E1 first (it's the root cause and benefits users who
   never click the button), then E2.
2. One commit is fine (small, coherent). Update `ARCHITECTURE.md` in the same commit:
   the "Signal sources" WMI reactive-trigger bullet (now wakes on fault *change*, not just
   appearance) and the "Tauri command surface" / `UiMsg` list (add `refresh_status` /
   `RefreshStatus`).
3. Bump version to **v0.25.3** across the four manifests + `Cargo.lock`, commit with the
   `[release]` marker, gate on CI, then roll the single release (delete the prior
   tag/release, tag `v0.25.3`, push) per CLAUDE.md.
