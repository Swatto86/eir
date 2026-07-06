# Eir — learning-loop deepening (L1–L5) (handover to Opus)

**Baseline:** v0.27.1 (`cca623d`), tree clean, synced with `origin/master`.
**Theme:** make Eir's *effectiveness* signal accurate — the half of the learning loop that
tells it whether a fix actually worked — without adding any new authority. This is the
"guardian that gets better at protecting *this* machine" work, kept strictly within the
existing **conservative-only invariant** (learning may only Skip / DeprioritiseMethod /
ConfidencePenalty — never enable, raise confidence, or unblock).

---

## The problem (grounded in the current code)

Eir already learns four conservative facts (`learn/`), and the load-bearing one for fixes is
**`FixIneffective`** — a fix TYPE that keeps "succeeding" but never helps gets a capped
confidence penalty. But its effectiveness measurement is coarse and often wrong:

1. **Global count, not the targeted fault.** `detect_fix_ineffective` (`learn/detect.rs:218`)
   judges a `ServiceRestart`/`ServiceStart` by whether the **global `failed_services` count**
   dropped (`after >= before` ⇒ ineffective). So a restart that genuinely clears service X is
   branded *ineffective* whenever an unrelated service Y newly fails in the same window — the
   count didn't drop, but the fix worked. It never asks "did the service this fix targeted
   actually recover?"
2. **Uncontrolled, shared after-measurement.** `feedback::update_after_states`
   (`feedback/mod.rs:35`) stamps the **current** global snapshot onto **every** pending row at
   whatever cycle happens to run — so a fix's "after" is measured at an arbitrary later time
   (maybe seconds after execution, before it settled; maybe conflated with a concurrent fix).
   ARCHITECTURE's own backlog names this ("after-state attribution is coarse… concurrent fixes
   share one after-snapshot").
3. **`improvement_score` ignores disk** (`feedback/mod.rs:90`) — a named gap; it only feeds the
   prompt's effectiveness display, but it's still wrong.

Net effect: the FixIneffective signal is noisy (false "ineffective" brands), and the prompt's
"RECENT EXECUTION FEEDBACK" summary the diagnostician reads is misleading. Fixing this makes
Eir's self-knowledge trustworthy — the prerequisite for every other learning improvement.

**Non-goal (explicitly out of scope):** auto-tuning `confidence_threshold` upward, promoting a
fix to auto-approve, or any positive/authority-raising learning. That would break the
conservative-only invariant the whole safety model rests on. This plan only makes the existing
*conservative* signals accurate.

---

## Ground rules

- **Conservative invariant preserved.** No new `Effect` variant; still only Skip /
  DeprioritiseMethod / ConfidencePenalty. Security actions stay carved out
  (`SECURITY_ACTION_TYPES`). The worst a wrong fact can still do is make Eir do *less*.
- **No wire change** — this is all internal (audit DB + learn/feedback). The migration is
  additive (`ALTER TABLE … ADD COLUMN`, nullable/defaulted, so an old DB upgrades cleanly).
- Pure logic gets unit tests (the score, the settle gate, `action_target`, the resolved-based
  detector). Gate before release: `cargo fmt --check`, `clippy --all-targets -D warnings`,
  `cargo test --workspace`, full tauri build via CI. Update `ARCHITECTURE.md` (the
  "Self-improvement" + "Persistence/feedback" sections + Known-limitations) in the same commit.

---

## L1 — Record what each fix targeted

- **Migration** (`migrations/0015_feedback_target.sql`): add to `execution_feedback`:
  `target TEXT NOT NULL DEFAULT ''`, `disk_before REAL`, `disk_after REAL`,
  `resolved INTEGER` (nullable: 1 = targeted fault cleared, 0 = not, NULL = not
  applicable/unmeasured).
- **`feedback::record`** (`feedback/mod.rs:7`) gains a `target: &str` param and a disk value;
  store `target` + `disk_before` (from the baseline `SystemState.disk_usage_percent`).
- **`action_target(&FixAction) -> String`** (new, in `feedback/mod.rs` or `models.rs`): returns
  the fault indicator a later `SystemState` can check — the `service_name` for
  `ServiceRestart`/`ServiceStart`/`ServiceStop`; `""` for everything else (targetless w.r.t.
  the metrics we collect). Keep it a small match; unit-test it.
- **Call site** (`main.rs:627`): `action` (the `FixAction`) is in scope in the executor task —
  pass `&feedback::action_target(&action)` and `baseline.disk_usage_percent`.

## L2 — Settle-windowed, per-target after-measurement

- **`feedback::update_after_states`** (`feedback/mod.rs:35`): fill a row only once a settle
  window has elapsed since it was recorded — `WHERE cpu_after IS NULL AND recorded_at <=
  <now - SETTLE_SECS>` (add `const SETTLE_SECS: i64 = 120`). Rows younger than that stay
  pending (measured a later cycle), so the "after" reflects a settled state, not a same-tick
  reading. For each filled row compute **`resolved`**: if `target` is non-empty (a service),
  `resolved = !state.failed_services.contains(&target)`; else `NULL`. Store `disk_after`,
  `resolved`, and the disk-inclusive `improvement_score`.
- This closes gap #2: each fix's outcome is now attributed to *that fix's own target*, measured
  after it settled, instead of the global snapshot stamped on everything at once.

## L3 — Effectiveness judged by the targeted outcome

- **`FeedbackRow`** (`learn/detect.rs:167`): carry `resolved: Option<bool>` (from the new
  column) instead of / in addition to the before/after counts.
- **`fix_feedback_rows`** (`learn/store.rs:43`): select `resolved`.
- **`detect_fix_ineffective`** (`learn/detect.rs:218`): judge each succeeded service-fix row by
  `resolved` — `Some(false)` ⇒ ineffective, `Some(true)` ⇒ effective, `None` ⇒ ignore. Keep the
  **zero-effective guard** (`effective == 0 && ineffective >= quorum`), `SERVICE_FIX_TYPES`
  restriction, quorum, and per-type aggregation. Now a type is penalised only if it *never*
  clears any target it's applied to — precise, and still can't suppress a healthy restart when
  some other target legitimately stayed broken.

## L4 — Honest effectiveness in the prompt + UI

- **`recent_summary`** (`feedback/mod.rs:127`): report the targeted outcome —
  `resolved = Some(true)` ⇒ "cleared the fault", `Some(false)` ⇒ "didn't clear it", `None` ⇒
  fall back to the score/"no measurable change" wording. So the diagnostician's "RECENT
  EXECUTION FEEDBACK" block (and, via the Learned card, the user) reason from real outcomes.
- **`improvement_score`** (`feedback/mod.rs:90`): add the disk delta
  (`disk_before - disk_after`, weight ~0.3 like cpu/mem) so the displayed score stops ignoring
  disk-space fixes. (Note the *detector* now keys off `resolved`, not this score, so the score
  is display/context only — but it should still be correct.)

## L5 — Tests

- `action_target`: service actions → the name; non-service → `""`.
- `improvement_score`: disk drop raises the score; a NULL before is treated as 0-delta (no
  panic), unchanged for cpu/mem.
- settle gate: a row younger than `SETTLE_SECS` is not filled; an older one is (test the SQL
  predicate via a small integration test against a temp DB, or factor the "is it settled?"
  check into a pure fn and test that).
- `detect_fix_ineffective` on `resolved`-based rows: all-`Some(false)` (≥quorum) ⇒ penalty;
  any `Some(true)` ⇒ no penalty (zero-effective guard); `None` rows ignored; a non-service type
  never penalised; the existing security carve-out still holds end-to-end.

---

## Adversarial sweep (before release)

Multi-lens + refute on the diff, focused on: the migration upgrading an existing DB cleanly
(old rows get `target=''`, `resolved=NULL` → treated as unmeasured, never mis-penalised); the
settle-window not stranding rows permanently; `resolved` correctly NULL for non-service actions
(so registry/disk/etc. fixes are never branded ineffective); the conservative invariant intact
(no path raises confidence/authority); and no regression to the existing learned-fact tests.

## Release

Patch or minor bump (behavioural refinement, no new feature/surface → **v0.27.2**) across the
four manifests + `Cargo.lock`; update `ARCHITECTURE.md` in the same commit; `[release]` marker;
CI green; roll the single release (delete prior tag/release, tag `v0.27.2`, push).
