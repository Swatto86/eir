# Eir — UI & user-facing bug-fix plan B1–B10 (handover to implementer)

**Baseline:** v0.24.0 (`6b53ed1`), tree clean, synced with `origin/master`.
**Theme:** UI and user-facing bugs only. Every item below was verified against the
source at the baseline commit — file/line references are to that tree. No new
features; the smallest correct fix in each case.

**Ground rules (apply to every item):**

- The frontend stays committed static vanilla HTML/CSS/JS (`ui/index.html`,
  `ui/main.js`) — no npm, no new JS dependencies. All service-supplied strings
  rendered into HTML go through `esc()`/`escAttr()`.
- The UI never constructs a `FixAction`; commands stay opaque ids over the pipe.
  Nothing here widens the pipe trust model.
- No wire-shape changes are needed anywhere in this plan; the one proto edit
  (B8) is a doc comment only.
- Rust changes get a unit test where the logic is testable without a live
  service (B1's command gate, B6's status formatter). Where a change can only
  be exercised live (B4), say so in the release notes.
- Gate before release: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, full
  tauri build via CI (`scripts/check-versions.ps1` gates the version sync).
  Adversarial sweep (multi-lens + refute) before tagging, per CLAUDE.md.
- Frontend changes have no automated harness — state plainly in the release
  notes which items are compile/reasoning-verified only.
- **Packaging:** all items in one patch release, **v0.24.1**. Bump the three
  `Cargo.toml`s + `eir-ui/tauri.conf.json`, re-sync `Cargo.lock`, update
  ARCHITECTURE.md + CONTEXT.md in the same `[release]` commit, tag, publish
  (single rolling release).

---

## P1 — correctness: user actions lost or dead controls

### B1 — Disconnected service: commands are silently dropped and Approve buttons die permanently

**What.** While the service is down (or restarting after a settings save), the
UI keeps the last status snapshot fully interactive. `pipe_client.rs:27-41`
overwrites only `status`/`error` on disconnect, so pending approvals, metrics,
and scan results stay on screen looking live. Any command clicked in that
window is accepted (`try_send` into the mpsc channel succeeds → the Tauri
command resolves Ok), then **silently discarded** by the queue drain at
`eir-ui/src/pipe_client.rs:46` when the connection cycle ends.

Concrete failure: click **Approve & run** while disconnected → `decide()`
(`ui/main.js:590-601`) disables the card's buttons, the invoke "succeeds", the
decision is dropped. Because `renderApprovals` only rebuilds on a signature
change (`ui/main.js:575-588`) and the pending set is unchanged after reconnect
(approvals are persisted service-side), the card is never re-rendered — **the
buttons stay disabled until the app restarts**, and the user believes they
approved a fix that never ran.

**Fix (both ends, both small):**

1. `eir-ui`: share an `Arc<AtomicBool>` connection flag with the pipe client
   (set true after `ClientOptions::open` succeeds in `connect_and_run`, false
   when it returns). Manage it as Tauri state; every command handler in
   `eir-ui/src/main.rs` checks it first and returns
   `Err("Eir service is not connected")` instead of queueing. The UI's
   existing catch paths then do the right thing: `decide()` re-enables the
   buttons, `submitAsk`/settings savers display "Failed: …".
2. `ui/main.js`: in `refreshInner`, compute
   `const svcDown = ['ServiceDisconnected','Connecting','Restarting'].includes(status.status)`;
   pass it into the renderers that already take `paused`
   (`renderDisk`/`renderStartup`) and use it the same way for the other action
   buttons (`upd-now`, `pause-btn`, approve/reject via a `body.svc-down` class
   + CSS `pointer-events:none; opacity:.5` on `.approval-actions button`).
   Give the disabled buttons a `title` explaining why. Optionally dim the
   metric cards under `body.svc-down` so frozen CPU/memory/disk numbers don't
   read as live.

**Verify.** Rust: extend the existing `pipe_client` test module — the flag
must flip true on connect and false after disconnect; a gated command handler
returns Err while false. JS: reasoning-verified; manual check is stopping
`EirSvc` and clicking around.

### B2 — Activity view repaints every 2 s: selection wiped, Undo's double-click guard defeated, tooltips flicker

**What.** `renderActivity` (`ui/main.js:659-676`) rebuilds `innerHTML`
unconditionally on every 2 s poll. Every sibling list renderer
(`renderApprovals`, `renderLearned`, `renderUpdater`, `renderAsk`,
`renderDisk`, `renderStartup`) has a signature guard precisely to avoid this —
the comments at `ui/main.js:570-573` and `:787-789` document why. Effects:

- Text selection in `.act-main` (`user-select: text`, `index.html:235`) is
  destroyed every 2 s — you cannot copy a diagnosis.
- The Undo button's in-flight `disabled` state (`ui/main.js:684`) is wiped by
  the next repaint, re-offering an Undo that the service will reject as
  already-claimed (harmless server-side, misleading client-side).
- Hover tooltips (`title` on truncated text) die every 2 s.

Same class of churn on the dashboard: `renderAiNow` (`ui/main.js:613-626`),
`renderDigest` (`:279-287`), the hero error line (`:214-216`), and
`renderUsage` (`:696-724`) assign `textContent`/`innerHTML` every poll even
when unchanged — `#ai-now-text`, `#digest-text`, and `#hero-err` are all
`user-select: text`, so selecting them is equally impossible.

**Fix.** Give `renderActivity` the established signature guard (sig over the
items array). Track in-flight undo ids in a small `Set` (mirror `decidingIds`)
and re-apply `disabled` after any rebuild. For the dashboard text nodes: write
only when the value changed (a tiny `setText(el, v)` helper doing
`if (el.textContent !== v) el.textContent = v`; use it in the four spots
above). Keep `renderUsage` behind a cheap sig too.

**Verify.** Reasoning-verified; manual: select activity text, watch it survive
polls.

### B3 — Relative ages freeze inside signature-guarded lists

**What.** The sig guards (correctly) skip re-rendering, but the rendered HTML
embeds `ago(ts)` at build time, so the ages never tick:

- Approval cards: `ago(info.created_at)` (`ui/main.js:548`) — sig is
  `id:created_at` (`:577`), so an approval that has sat for 3 hours still says
  "just now".
- Ask entries: `ago(e.at)` (`:360`), sig is the `at` list.
- Updater history rows: `ago(r.at)` (`:820`), sig is content.
- Activity `act-when` currently updates only because of the B2 bug — fixing B2
  without this would freeze it too.

**Fix (one helper, all lists).** Render every relative age as
`<span class="…" data-ts="${ts}">${ago(ts)}</span>`, and at the end of
`refreshInner` run a single sweep:
`document.querySelectorAll('[data-ts]').forEach(el => { el.textContent = ago(+el.dataset.ts); })`.
Remove per-renderer age plumbing where it becomes redundant. (`ago(0)` already
returns `''`, so a zero timestamp stays blank.)

**Verify.** Reasoning-verified; manual: leave an approval pending >1 min,
watch the age advance without a re-render.

---

## P2 — tray & window behaviour

### B4 — Clicking the tray icon (or "Open Status") does not restore a minimized window

**What.** Both the tray-icon left-click handler
(`eir-ui/src/main.rs:575-587`) and the "Open Status" menu handler
(`:559-564`) call only `w.show()` + `w.set_focus()`. On Windows, neither
restores a **minimized** window — so minimize the window, click the tray icon,
and nothing appears (at best the taskbar flashes). Hidden→shown works;
minimized doesn't.

**Fix.** Add `let _ = w.unminimize();` between `show()` and `set_focus()` in
both handlers.

**Verify.** Compile-verified (behavioural check needs a live run — flag it).

### B5 — Tray "Pause Monitoring" menu label never changes

**What.** `pause_item` is created once with static text
(`eir-ui/src/main.rs:545-546`) and never updated. When monitoring is paused,
the menu still reads "Pause Monitoring" — the user can't tell whether the next
click pauses or resumes. (The window's button relabels correctly,
`ui/main.js:231`.)

**Fix.** The 500 ms tray-sync loop (`eir-ui/src/main.rs:605-615`) already
polls the shared status; extend its snapshot to `(status, paused)`, clone
`pause_item` into the loop, and on change call
`let _ = pause_item.set_text(if paused { "Resume Monitoring" } else { "Pause Monitoring" });`.

**Verify.** Compile-verified; the loop change is trivial to eyeball.

### B6 — Tray tooltip shows raw CamelCase status

**What.** `update_tray` formats the tooltip as `"Eir — {status}"`
(`eir-ui/src/main.rs:369`), producing "Eir — PendingApproval" and
"Eir — ServiceDisconnected". The window header already humanises the same
string (`ui/main.js:208-209`).

**Fix.** Add a small `fn friendly_status(s: &str) -> String` in `eir-ui` that
inserts a space before interior uppercase letters (mirror the JS regex), use it
in the tooltip. Unit-test it (`PendingApproval` → `Pending Approval`,
`Active` → `Active`, `ServiceDisconnected` → `Service Disconnected`).

**Verify.** Unit test.

### B7 — No minimum window size

**What.** `eir-ui/tauri.conf.json` sets `width`/`height` (1000×680) but no
minimum; the window can be dragged down to a sliver where the 168 px sidebar
plus content grids collapse into an unusable mess.

**Fix.** Add `"minWidth": 720, "minHeight": 480` to the window config.

**Verify.** Config-only; compile/bundle-verified.

---

## P3 — smaller user-facing paper cuts

### B8 — Toggling Ignore on an app silently deletes its per-app updater note

**What.** The UI always sends `note: ''` with `set_app_ignore`
(`ui/main.js:843`), and the service treats an empty note as "remove"
(`eir-svc/src/main.rs:1766-1771`) — so a note hand-set in `config.toml` (the
hint mechanism the AI-native updater reads) is wiped by any Ignore/Unignore
click. The UI has no notes editor, so the wire's clear-note path is reachable
*only* by accident.

**Fix.** Service-side one-liner: only touch `cfg.updater.notes` when the
trimmed note is non-empty (drop the `remove` branch). Clearing a note is done
where notes are set — `config.toml`. Document the rule on the `UiMsg::SetAppIgnore`
doc comment in `eir-proto` (comment change only; wire shape is unchanged).

**Verify.** Compile-verified; if the ui_rx handler has a test seam, add a case
(ignore-toggle with empty note preserves an existing note); otherwise note it
as compile-verified.

### B9 — Ask Eir: previous error flashes back after submitting, and Ctrl+Enter is undiscoverable

**What.** (a) `submitAsk` shows "Sending…" (`ui/main.js:370`), but the
immediate `refresh()` plus the next poll re-derive the status line as
`running ? '' : ask.error` (`:350`) — until the service flips `running`
(≤2 s), a *previous* question's error is redisplayed, reading as if the new
question failed instantly. (b) Ctrl/Cmd+Enter submits (`:381-384`) but nothing
in the UI says so.

**Fix.** (a) On successful submit, record the currently-displayed error as a
baseline (`askErrBaseline`, plus an `askSentAt` timestamp); in `renderAsk`,
while not `running`, suppress an error identical to the baseline for a few
seconds (clear the baseline once `running` is observed or the error text
changes — a genuinely new rejection like "Please wait a few seconds between
questions" differs from the baseline and shows immediately). (b) Append
"(Ctrl+Enter to send)" to the textarea placeholder in `index.html:399`.

**Verify.** Reasoning-verified.

### B10 — Copy fixes: About-view update check, Kilo usage note

**What.** (a) The About view's "Check for updates" shows "Checking…" for the
entire download+install (up to 10 min, `eir-ui/src/main.rs:249-272`) before
the app restarts itself — it looks hung. (b) The AI-usage card's note for
`kilo_cli` falls through to "Provider-reported cost where available."
(`ui/main.js:705-711`) even though the provider hint sells Kilo as
subscription-based; unlike `claude_cli`, the cost cells stay populated.

**Fix.** (a) Change the pre-check text set in `ui/main.js:1114` to
"Checking… if an update is found it will download and restart automatically
(this can take a few minutes)." — copy only, no progress plumbing.
(b) Add a `kilo_cli` note: "Kilo-reported cost — usually covered by your Kilo
plan; real for BYOK models." Keep the cost cells as-is (Kilo BYOK cost is
real, so a `claude_cli`-style dash would be wrong).

**Verify.** Copy only.

---

## Release chore (same `[release]` commit)

- ARCHITECTURE.md: correct the stale "currently `0.22.1` in every `[package]
  version`" sentence (Workspace section) to the released version; describe the
  new pipe-client connected-flag seam (B1) and the `data-ts` age-refresh
  pattern (B3) in the "Pipe protocol & tray UI" section.
- CONTEXT.md: session note for the v0.24.1 bug-fix release.
- Version bump v0.24.1 everywhere + `Cargo.lock` sync, `[release]` marker,
  tag, single rolling release.

## Considered and rejected

- **Out-of-range settings inputs (e.g. decision interval 5 s):** the service
  clamps everything on apply (`eir-svc/src/config.rs:270-277`); the UI showing
  the unclamped value until the restarted service re-broadcasts is a cosmetic
  non-issue.
- **Dashboard metrics showing NaN on missing fields:** `StatusPayload` derives
  `Default`; numeric fields are always present on the wire.
- **`decide()` leaving `decidingIds` stale:** ids are removed in `finally`;
  the permanent-disable failure is the B1 disconnect path, fixed there.
- **Poisoned-mutex `unwrap()`s in the notify/tray loops:** only reachable
  after another thread already panicked while holding the status lock;
  `get_status` recovers and the rest is a cascade we don't need to gold-plate.
