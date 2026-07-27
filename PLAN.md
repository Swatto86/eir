# Eir roadmap — v0.33 burn-in and v0.34 Trust Loop

**Current release:** v0.33.0

## Now: burn in v0.33

Use Eir normally for several days and let at least one scheduled updater cycle complete.
Do not manufacture dangerous faults or approve disruptive actions solely to exercise them.

Watch for:

- **Upgrade integrity:** About shows UI and service version `0.33.0`; no mismatch remains
  after the service reconnects.
- **Provider path:** Settings → **Test provider** succeeds through the installed
  LocalSystem service. Provider/model changes apply live; collector-setting changes return
  a restart result and reconnect.
- **Updater truthfulness:** Current / Verified / Failed evidence matches the observed
  result. Last run and last clean run survive restart, visible degraded coverage remains
  a warning, and Clear hides displayed history without resetting scheduling fairness.
- **Durability:** naturally occurring approvals survive restart. If a safe registry undo
  is encountered, it restores only when the live value still matches Eir's write.
- **Signal honesty:** collector freshness and errors remain visible; a failed probe does
  not become a healthy zero.
- **Game Mode:** scheduled work pauses and resumes with the documented lease/latch
  behaviour.

For a burn-in issue, capture the time, the visible message, UI/service versions, and the
small relevant excerpt from `eir.log`. Do not include API keys or `config.toml`.

The burn-in is complete when normal use has covered an upgrade, a LocalSystem provider
test, and a scheduled updater cycle without an unexplained outage, lost approval, or false
clean label that contradicts the cycle's visible evidence.

## Next: v0.34 Trust Loop

Priority order:

1. **Upgrade CI gate.** Install the previous release, seed representative config/database
   state, upgrade to the candidate, and prove migrations, preserved settings, service
   restart, and a correlated protocol-v2 command on the exact release commit.
2. **Structured action receipts.** Persist accepted → executed → verified outcomes and add
   guarded undo for startup and task toggles. Undo must compare live state with Eir's
   applied state before restoring; no generic undo abstraction.
3. **Better learning inputs.** Feed verified executor postconditions into conservative
   learning beyond service fixes. Learning may still only skip, deprioritise, or reduce
   confidence—never expand authority.
4. **Updater publisher identity.** Anchor expected publisher identity in signed installed
   software or a curated local mapping rather than an AI claim. Keep Scoop disabled under
   LocalSystem unless a safe active-user broker is justified.
5. **Versioned config migration.** Add an explicit schema version and preserve unknown keys
   across saves before configuration evolves further.

Only add a redacted diagnostic export if burn-in shows that manual evidence collection is
materially inadequate.

## Non-goals during burn-in

- No new repair-action families or wider auto-approval.
- No remote-control surface, plugin system, or general-purpose shell authority.
- No policy auto-tuning that can make Eir more aggressive.

## Release gate

Each behavioural fix starts with a failing regression test. A candidate is ready only when
formatting, clippy, all-target tests, version checks, fresh-install and upgrade service
gates, the packaged WebView workflow, and standalone executable smoke tests pass on the
exact commit to be tagged.
