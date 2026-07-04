# Eir — Feature plan F10–F13 (handover to implementer)

**Baseline:** v0.23.1 (`488748b`), tree clean, synced with `origin/master`.
**Theme:** user-facing value. Eir is now a solid autonomous guardian; these four
features make it something the user *opens on purpose*. Everything is SSD-era:
no defrag, no disk-optimisation theatre — space, boot time, and answers instead.

**Ground rules (apply to every feature):**

- New `StatusPayload` fields are `#[serde(default)]` (backward-compat invariant,
  `eir-proto/src/lib.rs` — see ARCHITECTURE.md "Wire types").
- New `UiMsg` variants follow the existing `#[serde(tag = "type", rename_all = "snake_case")]` enum.
- The UI never constructs a `FixAction` and the service never trusts an action
  from the wire. Where a feature offers a "fix it" button, the UI sends an
  opaque entry id; the service maps id → action from **its own** last scan
  results, then routes through `pol.evaluate` + the normal exec path. The pipe
  is writable by any authenticated user — that trust model must not widen.
- Long work runs off-loop, mirroring the analysis-task pattern
  (`eir-svc/src/main.rs` ~1435: inner `tokio::spawn` under `tokio::time::timeout`,
  result over a dedicated mpsc, guard flag released even on panic/hang).
- Every AI call goes through the existing `AiClient` and logs its `CallUsage`
  into the usage accounting so it appears in the AI-usage card.
- All service-supplied strings rendered in the UI go through `esc()`/`escAttr()`
  (`ui/main.js`). No new JS dependencies — the frontend stays committed static
  vanilla HTML/CSS/JS, no npm.
- Per release: version bump in the three `Cargo.toml`s + `eir-ui/tauri.conf.json`,
  re-sync `Cargo.lock` (`scripts/check-versions.ps1` gates CI), update
  ARCHITECTURE.md + CONTEXT.md in the same commit, `[release]` marker, then tag.
- Gate before each release: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, full
  tauri build via CI. Adversarial sweep (multi-lens + refute) before tagging,
  per CLAUDE.md.

**Suggested packaging:** v0.24.0 = F10 + F11 (fast, low-risk, immediately
visible). v0.25.0 = F12 + F13 (each adds a scan subsystem; keep the sweeps small).

---

## F10 — "Ask Eir": free-text questions answered with live system context

**What.** A new sidebar view where the user types a question — "why is my PC
slow right now?", "what was that error notification about?", "is my disk OK?" —
and gets a plain-English answer grounded in the *current* signal snapshot,
recent problems/executions, learned facts, and the resource trend. Diagnostic
answer only: the reply proposes no actions and nothing is parsed or executed
from it. Fixes still come only from the normal decision cycle.

This is the flagship: it turns the whole existing signal + AI stack into
something interactive, for one bounded `complete_text` call per question.

### Wire (`eir-proto/src/lib.rs`)

- `UiMsg::AskEir { question: String }`.
- `StatusPayload.ask: Option<AskStatus>` (`#[serde(default)]`).
  `AskStatus { running: bool, error: Option<String>, entries: Vec<AskEntry> }`;
  `AskEntry { question: String, answer: String, at: i64 }`.

### Service (`eir-svc/src/main.rs` + small `ask.rs` for the prompt builder)

- `SvcState` gains `ask_running: bool` and `ask_entries: VecDeque<AskEntry>`
  (cap 10, newest first — mirror `push_problem`). Memory-only; history is lost
  on service restart, which is acceptable (state, not audit data).
- `ui_rx` arm for `AskEir`:
  - Reject (set `AskStatus.error`, broadcast) when: question empty/whitespace,
    `> 1_000` chars, `ai` is `None`, `ask_running`, or the last ask finished
    `< 15 s` ago (spend guard — the pipe is writable by any local user).
  - Otherwise set `ask_running = true`, broadcast, and spawn the off-loop task:
    build the prompt, call `ai.complete_text(prompt, "")` (main model, no web
    search, no diagnosis system prompt — same entry point the labeller and
    digest use, `ai/client.rs:880`), under `timeout(ASK_MAX = 4 min)` with the
    inner-spawn panic isolation. Send `Result<(String, Option<CallUsage>), String>`
    over a new `ask_done_rx` select arm.
  - `ask_done_rx` arm: clear `ask_running`, log usage (same path as
    labeller/digest usage), push an `AskEntry` (or set `error`), broadcast.
    Do **not** touch `st.error` (same isolation as `exec_done_rx`).
- Prompt (in `ask.rs`, pure fn, unit-testable): current metrics + failed
  services + resource-trend note + last N recent problems/executions summaries
  + active learned facts + the question. Instructions: answer in plain English
  for a non-technical user, ≤ 300 words, diagnostic only — explicitly "do not
  propose registry edits, commands, or actions; Eir applies fixes through its
  own policy engine". Truncate each context section with the existing caps so
  the prompt stays bounded.

### UI (`ui/index.html`, `ui/main.js`)

- New sidebar item **Ask Eir** with a textarea (maxlength 1000), a Send button
  (disabled while `ask.running` or provider unconfigured — mirror the
  update-now gating pattern), a spinner line while running, and the entry list
  rendered newest-first. Show `ask.error` inline. Everything through `esc()`.

### Tests

- Prompt builder: unit tests that each context section appears, caps hold, and
  the no-actions instruction is present.
- Gating: unit test the reject conditions as a pure fn
  (`ask_rejection_reason(&state, &question, now) -> Option<&str>`), table-style.

### Risks / refutations

- *Spend abuse via the open pipe* — bounded by the in-flight guard + 15 s gap +
  1 000-char cap; each call is one non-web completion. Accepted.
- *Answer drifts into instructions the user might run by hand* — prompt forbids
  it; answer is display-only, never parsed. Residual risk is the same as any
  chat assistant. Accepted.
- *CLI providers* — `complete_text` already routes them; nothing new.

---

## F11 — Health timeline: render `system_state_history` on the dashboard

**What.** `system_state_history` is written every cycle and read only by the
trend summariser — the user never sees it. Add 24-hour sparkline charts
(CPU / memory / disk) to the Dashboard with markers where problems were found
and fixes ran. Zero AI cost, zero risk, uses data Eir already has. This is the
"is my machine actually healthier?" view the weekly digest talks about.

### Service

- `audit.rs`: `get_state_history(pool, since_unix) -> Vec<MetricPoint>` reading
  `(created_at, cpu, memory, disk)` from `system_state_history`, ordered
  ascending, bucketed/downsampled to ≤ 200 points (rows are ~1/10 min, so a
  24 h window is ~144 rows — downsampling is just a safety cap; a plain
  `LIMIT`-free query + Rust thinning is fine).
- `SvcState` caches `history: Vec<MetricPoint>`; refresh **once per decision
  tick** (in the per-cycle body, not per broadcast — broadcasts happen on every
  state change and must stay cheap).

### Wire

- `MetricPoint { at: i64, cpu: f32, memory: f32, disk: f32 }` in `eir-proto`.
- `StatusPayload.history: Vec<MetricPoint>` (`#[serde(default)]`). ~150 points
  ≈ a few KB per snapshot — acceptable on a local pipe.

### UI

- Dashboard section "Last 24 hours": three inline **SVG** sparklines (hand-built
  polylines — no chart library, consistent with the no-dependency frontend).
  Overlay markers from `recent_problems` / `recent_executions` timestamps
  already in the payload (dots on the x-axis; tooltip via `<title>`).
- Y-axis fixed 0–100 %, colour the disk line with the existing status accents
  when > 90.

### Tests

- `get_state_history` round-trip against the real migrations (insert synthetic
  rows, assert ordering + thinning) — mirror the existing audit test style.
- Downsampling: pure-fn unit test (input > 200 points → ≤ 200, endpoints kept).

### Risks / refutations

- *Payload bloat* — capped at 200 points, refreshed per tick. Refuted.
- *Table growth* — ~52k rows/year at the default cadence; no pruning needed now.
  Note it in ARCHITECTURE.md's backlog rather than adding pruning speculatively.

---

## F12 — Disk-space insights: "what's eating my SSD", with policy-gated cleanup

**What.** SSDs make *space* the scarce resource, not fragmentation. An
on-demand scan (button, like Update now) produces a ranked list of space
consumers: known reclaimable locations (temp dirs, browser caches, Windows
update leftovers, crash dumps, `Windows.old`, hibernation file, package-manager
caches, Recycle Bin) plus the largest top-level directories under `C:\` and
each user profile. Each entry gets a deterministic category + hand-written note
(trustworthy, like `explain.rs`); entries that map to a safe existing action
get a **Clean** button routed through the normal policy gate.

### Wire

- `UiMsg::ScanDisk` and `UiMsg::CleanDiskEntry { id: String }`.
- `StatusPayload.disk_insights: Option<DiskInsightsView>` (`#[serde(default)]`;
  note `disk: f32` already exists — do not collide).
  `DiskInsightsView { running: bool, scanned_at: i64, error: Option<String>,
  entries: Vec<DiskEntryView> }`;
  `DiskEntryView { id: String, path: String, size_bytes: u64, category: String,
  note: String, cleanable: bool }`.

### Service — new `eir-svc/src/disk_scan.rs`

- **Scan (off-loop, mirror `spawn_update_cycle`:** detached task, inner spawn +
  `timeout(SCAN_MAX = 5 min)`, done-channel arm clears `running` even on
  panic/hang; `scan_running` flag; `ScanDisk` gated on `!running && !paused`).
- Deterministic targets, each with a hard-coded category + note (this is the
  `explain.rs` philosophy — the text the user trusts is never AI-authored):
  - Per-user + `C:\Windows\Temp` temp dirs; `C:\Windows\SoftwareDistribution\Download`;
    crash dumps (`C:\Windows\Minidump`, `MEMORY.DMP`); `C:\Windows.old`;
    `hiberfil.sys`/`pagefile.sys` (report-only); browser caches (Chrome/Edge/
    Firefox default profile cache dirs per user); package caches (`%LOCALAPPDATA%`
    npm/pip/cargo/NuGet when present); Recycle Bin (`$Recycle.Bin` per drive,
    du-style); WSL/Docker `.vhdx` (report-only).
  - Plus: top-level directory sizes for `C:\` and each `C:\Users\<user>` (depth
    ≤ 2, `walkdir` with per-entry error tolerance — LocalSystem can read most
    of it; skip reparse points to avoid cycles and double-counting).
  - Keep the top ~25 entries by size; ids are stable hashes of the path.
- **Cleanup mapping (server-side only).** `CleanDiskEntry { id }` looks the id
  up in the service's own last scan (unknown/stale id → ignore + note). The
  entry's category maps to an existing `FixAction`:
  - temp/prefetch → `DiskCleanup { target }` (whitelisted → auto-runs);
  - log/dump dirs → `LogCleanup { path, days_old: 7 }` (whitelisted; the
    v0.23.0 canonicalised-root + protected-dir guards already apply);
  - single large file in a safe location → `FileDelete { path }` (off-whitelist
    → approval card, with the existing `file_facts` risk classification);
  - report-only categories (`hiberfil.sys`, WinSxS, `.vhdx`, Recycle Bin) →
    `cleanable: false`, no action. (Recycle Bin emptying, `powercfg /h off`,
    and DISM component cleanup are deliberately out of scope v1 — each is a new
    action with its own blast radius; add later if the entries prove popular.)
  - The mapped action goes through `pol.evaluate` + `safety::rate_limited` +
    `in_flight` dedupe and the executor worker — **identical routing to an
    AI-proposed fix**, reason `"user-requested cleanup"`.
- No AI call in v1. (An optional later pass could annotate *unknown* large
  directories with one `complete_text` call; the deterministic notes cover the
  common cases, so don't build it yet.)

### UI

- New sidebar view **Disk** (or a Dashboard card + view): Scan button (disabled
  while running, mirror Update-now), scanned-at line, entries as rows — path,
  human size, category chip, note, Clean button when `cleanable`. Clean click →
  `clean_disk_entry(id)`; row state updates on the next poll (approvals appear
  in the Approvals view as usual).

### Tests

- Category mapping: unit table — every category maps to the intended
  `FixAction` variant or to `cleanable: false`; no category maps to anything
  off this list.
- Size scan: unit test the walker on a temp fixture tree (depth cap respected,
  reparse points skipped, unreadable entries tolerated).
- Id lookup: stale/unknown id is a no-op.

### Risks / refutations

- *UI-triggered deletion widening the trust model* — refuted: ids map to
  service-derived actions, policy-gated exactly like AI proposals; `FileDelete`
  still requires approval; blocklists still apply.
- *Scanning C:\ as SYSTEM takes minutes* — bounded by depth cap + 5-min
  timeout; it's on-demand, off-loop, and the loop stays responsive by design.
- *Double-count via junctions* — skip reparse points (the v0.23.1 LogCleanup
  canonicalisation bug is the cautionary tale; state this in a comment).

---

## F13 — Startup advisor: what launches at logon, what it costs, one-click disable

**What.** Enumerate everything that starts at logon — Run/RunOnce registry
entries (HKLM + HKCU, incl. Wow6432Node), Startup folders (per-user + common),
and logon-triggered scheduled tasks — with current enabled/disabled state from
`StartupApproved`. One bounded AI call classifies each entry (`keep` /
`optional` / `unnecessary` + a one-line plain-English "what this is"). The user
can disable an entry with one click, Task-Manager-style (write the
`StartupApproved` flag — fully reversible), routed through the approval flow.

### Wire

- `UiMsg::ScanStartup` and `UiMsg::SetStartupEntry { id: String, enable: bool }`.
- `StatusPayload.startup: Option<StartupView>` (`#[serde(default)]`).
  `StartupView { running: bool, scanned_at: i64, error: Option<String>,
  entries: Vec<StartupEntryView> }`;
  `StartupEntryView { id, name, command, location, enabled: bool,
  verdict: String, note: String }` (verdict/note empty when AI unconfigured —
  the deterministic listing is useful on its own).

### Service — new `eir-svc/src/startup_scan.rs` + one new FixAction pair

- **Enumerate (off-loop, same scaffold as F12):** registry Run keys via the
  existing PowerShell-with-timeout helper (`executor/powershell.rs` pattern) or
  direct registry reads; Startup folder `.lnk`s; `Get-ScheduledTask` filtered to
  logon triggers. Read `HKCU\...\Explorer\StartupApproved\{Run,StartupFolder}`
  (and HKLM equivalent) to report the current enabled/disabled state — first
  byte `0x02` = enabled, `0x03` = disabled.
- **Classify:** one `complete_text` call with the entry list (name, command
  path, signer if cheaply available), returning strict JSON
  `[{id, verdict, note}]`; parse defensively (reuse the `extract_json_object`
  approach), unknown ids dropped, missing verdicts default to `optional`. AI
  text is advisory display only — it triggers nothing.
- **New actions** in `models.rs` (+ `explain.rs` arms, executor adapter):
  - `StartupDisable { name: String, location: String }` /
    `StartupEnable { name, location }` — write the `StartupApproved` binary
    flag via `Set-ItemProperty -Type Binary` through the timeout helper.
    `location` is one of a **closed set** (`hkcu_run`, `hklm_run`,
    `startup_folder`, …) mapped to hard-coded key paths in the adapter — the
    wire value is a selector, never a raw registry path.
  - Deliberately **not** added to the AI prompt's action catalogue — these are
    user-initiated only. Not whitelisted in `policy.toml`, so `pol.evaluate`
    lands them on `RequireApproval` automatically; `explain.rs` marks them
    `reversible: true`. Scheduled-task entries reuse the existing
    `TaskDisable`/`TaskEnable` instead.
  - `SetStartupEntry` maps id → entry from the service's own last scan (same
    server-side-mapping rule as F12) and routes the derived action through the
    normal gate.

### UI

- New sidebar view **Startup**: Scan button, entries grouped by location, state
  pill (enabled/disabled), verdict chip colour-coded, note text, Disable/Enable
  button → `set_startup_entry(id, enable)`. Disabled entries render dimmed.

### Tests

- `StartupApproved` flag encode/decode: unit table (enabled bytes, disabled
  bytes, unknown/absent → treated enabled — that's Windows' default).
- Location selector → key path mapping: closed-set unit table; unknown selector
  is an error, not a passthrough.
- Classification JSON parsing: valid, partial, and garbage inputs.

### Risks / refutations

- *Disabling something needed at logon* — mitigated: approval-gated, reversible
  by construction (re-enable writes `0x02`), Task-Manager-equivalent mechanism
  (no entry is deleted, ever), and the AI verdict is advisory only.
- *Registry writes from a UI click* — the wire carries only an id + a closed
  location selector; the adapter owns the real paths. No raw-path surface.
- *HKLM StartupApproved needs admin* — the service is LocalSystem; fine. The
  per-user key for other users' HKCU is out of scope v1 (scan the interactive
  user's hive via the loaded profile only; note the limitation in the view).

---

## Sequencing

1. **F11** (timeline) — smallest, zero-risk, exercises the payload/UI seam.
2. **F10** (Ask Eir) — flagship; reuses the analysis-task scaffold.
3. → **release v0.24.0** (bump ×4 + lock, docs, sweep, tag).
4. **F12** (disk insights) — scan scaffold + server-side action mapping.
5. **F13** (startup advisor) — reuses F12's scaffold + mapping pattern; adds
   the one new FixAction pair.
6. → **release v0.25.0**.

Verification level expected in the handback: compile-verified (fmt + clippy
`--all-targets` + `cargo test --workspace` + full tauri build) with the
adversarial sweep run per release. Live-run checks that need a real machine
(scan timings, StartupApproved byte layout on the target Windows build, toast
notifications) should be flagged explicitly, not implied.
