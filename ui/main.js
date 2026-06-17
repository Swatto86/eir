const invoke = window.__TAURI__.core.invoke;

// Describe which provider/model the "Other Updates" web check will use. It
// follows the configured provider: OpenRouter free model + web plugin, or the
// Claude CLI (update_check_model, blank = haiku).
function updateCheckLabel(s) {
  if (!s) return '';
  if (s.provider === 'openrouter') {
    return `OpenRouter · ${s.model || 'openrouter/free'} + web`;
  }
  const m = (s.update_check_model || '').trim();
  const lower = m.toLowerCase();
  const isClaude = ['haiku', 'sonnet', 'opus'].includes(lower) || lower.startsWith('claude');
  return `Claude CLI · ${isClaude ? m : 'haiku'}`;
}

const STATUS_COLORS = {
  Active:               'var(--green)',
  Warning:              'var(--yellow)',
  PendingApproval:      'var(--orange)',
  Executing:            'var(--blue)',
  Error:                'var(--red)',
  ServiceDisconnected:  'var(--red)',
  Connecting:           'var(--gray)',
  Paused:               'var(--gray)',
  Initializing:         'var(--gray)',
};

// Hide on close (tray remains active)
window.__TAURI__.window.getCurrentWindow().onCloseRequested((e) => {
  e.preventDefault();
  window.__TAURI__.window.getCurrentWindow().hide();
});

function pct(v) { return `${Math.round(v)}%`; }

// Relative age from a unix-seconds timestamp (0/missing → blank).
function ago(ts) {
  if (!ts) return '';
  const s = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function barColor(v) {
  if (v >= 90) return 'var(--red)';
  if (v >= 75) return 'var(--yellow)';
  return 'var(--blue)';
}

function setBar(barId, value) {
  const el = document.getElementById(barId);
  el.style.width = `${Math.min(value, 100)}%`;
  el.style.background = barColor(value);
}

function problemTag(p) {
  if (p.blocked)       return '<span class="tag tag-block">Blocked</span>';
  if (p.auto_executed) return '<span class="tag tag-auto">Auto</span>';
  return `<span class="tag tag-warn">${Math.round(p.confidence * 100)}%</span>`;
}

function exTag(e) {
  return e.success
    ? '<span class="tag tag-ok">OK</span>'
    : '<span class="tag tag-block">Failed</span>';
}

function renderList(containerId, items, rowFn, emptyText) {
  const el = document.getElementById(containerId);
  if (!items || items.length === 0) {
    el.innerHTML = `<div class="empty">${emptyText}</div>`;
    return;
  }
  el.innerHTML = items.map(rowFn).join('');
}

// Build one approval card. The "What this will do" block and the Reversible/
// Irreversible flag come from the service's deterministic explainer, so they are
// trustworthy; side-effects/undo are the AI's supporting commentary.
function approvalCard(info) {
  const flag = info.reversible
    ? '<span class="tag tag-ok">Reversible</span>'
    : '<span class="tag tag-block">Irreversible — cannot be undone</span>';
  const details = info.target_details
    ? `<pre class="appr-details">${esc(info.target_details)}</pre>`
    : '';
  const grid = `
    <span class="label">Diagnosis</span>    <span class="val">${esc(info.diagnosis)}</span>
    <span class="label">Root cause</span>   <span class="val">${esc(info.root_cause)}</span>
    <span class="label">Confidence</span>   <span class="val">${Math.round(info.confidence * 100)}%</span>
    <span class="label">Why approval</span> <span class="val">${esc(info.reason)}</span>
    <span class="label">Side effects</span> <span class="val">${esc(info.side_effects)}</span>
    <span class="label">Undo</span>         <span class="val">${esc(info.undo_instructions)}</span>`;
  return `
    <div class="approval-card" data-approval-id="${info.id}">
      <h2>⚠ Approval needed<span class="appr-age">${ago(info.created_at)}</span></h2>
      <div class="appr-what">
        <div class="appr-what-label">What this will do</div>
        <div class="appr-what-text">${esc(info.action_summary || info.action)}</div>
        <div class="appr-flags">${flag}</div>
      </div>
      <div class="appr-target">
        <span class="appr-target-label">Target</span>
        <code class="appr-target-val">${esc(info.target || '—')}</code>
        ${details}
      </div>
      <div class="approval-grid">${grid}</div>
      <div class="approval-actions">
        <button class="btn-approve" data-id="${info.id}">Approve &amp; run</button>
        <button class="btn-reject"  data-id="${info.id}">Reject</button>
      </div>
    </div>`;
}

function renderApprovals(list) {
  const el = document.getElementById('approvals');
  el.innerHTML = (list && list.length) ? list.map(approvalCard).join('') : '';
}

function esc(s) {
  return String(s ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function fmtTokens(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K';
  return String(n);
}

let gbpRate = 0.79; // USD→GBP; refreshed from gbp_per_usd on load
function fmtGbp(usd) { return '£' + ((usd || 0) * gbpRate).toFixed(2); }

function renderUsage(u) {
  const card = document.getElementById('usage-card');
  if (!u) { card.style.display = 'none'; return; }
  card.style.display = 'block';
  const provider = (lastStatus && lastStatus.settings && lastStatus.settings.provider) || '';
  // Free models (OpenRouter free) and the Claude subscription incur no charge,
  // so don't show a misleading dollar figure for them.
  const free = provider === 'openrouter' || provider === 'claude_cli';
  const costCell = c => free ? '—' : fmtGbp(c);
  const note = provider === 'openrouter'
    ? 'Free model — no cost. Token counts shown for transparency.'
    : provider === 'claude_cli'
      ? 'No charge — uses your Claude subscription. Token counts shown for transparency.'
      : 'Estimated pay-as-you-go API cost.';
  document.getElementById('usage-body').innerHTML = `
    <div class="usage-grid">
      <div></div><div class="usage-h">Last 24h</div><div class="usage-h">Last 7 days</div>
      <div class="usage-l">Calls</div>
      <div class="usage-v">${u.calls_today}</div><div class="usage-v">${u.calls_week}</div>
      <div class="usage-l">Tokens</div>
      <div class="usage-v">${fmtTokens(u.tokens_today)}</div><div class="usage-v">${fmtTokens(u.tokens_week)}</div>
      <div class="usage-l">Est. cost</div>
      <div class="usage-v">${costCell(u.cost_today_usd)}</div><div class="usage-v">${costCell(u.cost_week_usd)}</div>
    </div>
    <div class="usage-note">${note}</div>
  `;
}

let lastStatus = null;

async function refresh() {
  let status;
  try { status = await invoke('get_status'); }
  catch (e) { console.error('get_status failed', e); return; }
  lastStatus = status;

  // Status dot + text
  const dot = document.getElementById('status-dot');
  const txt = document.getElementById('status-text');
  dot.style.background = STATUS_COLORS[status.status] ?? 'var(--gray)';
  txt.textContent = status.error
    ? `Error: ${status.error}`
    : status.status.replace(/([A-Z])/g, ' $1').trim();

  // Live model label
  const ml = document.getElementById('model-label');
  if (status.settings) {
    const s = status.settings;
    ml.textContent = s.model ? `${s.provider} · ${s.model}` : `${s.provider} · default model`;
    ml.title = `Other Updates check: ${updateCheckLabel(s)}`;
  } else {
    ml.textContent = '';
  }

  // Pause button label
  document.getElementById('pause-btn').textContent =
    status.paused ? 'Resume' : 'Pause';

  // Metrics
  document.getElementById('cpu').textContent    = pct(status.cpu);
  document.getElementById('memory').textContent = pct(status.memory);
  document.getElementById('disk').textContent   = pct(status.disk);
  setBar('cpu-bar',  status.cpu);
  setBar('mem-bar',  status.memory);
  setBar('disk-bar', status.disk);

  // Failed services
  const svcCard = document.getElementById('services-card');
  const svcList = document.getElementById('services-list');
  if (status.failed_services && status.failed_services.length > 0) {
    svcCard.style.display = 'block';
    svcList.innerHTML = status.failed_services
      .map(s => `<span class="svc-chip">${esc(s)}</span>`)
      .join('');
  } else {
    svcCard.style.display = 'none';
  }

  // Pending approvals (persistent queue — one card each)
  renderApprovals(status.pending_approvals);

  // Problems
  renderList(
    'problems-list',
    status.recent_problems,
    p => `<div class="row">
      <div class="row-title">${problemTag(p)}<span>${esc(p.diagnosis)}</span><span class="row-age">${ago(p.at)}</span></div>
      <div class="row-sub">${esc(p.action)}${p.reason ? ' — ' + esc(p.reason) : ''}</div>
    </div>`,
    'No problems detected yet'
  );

  // Executions
  renderList(
    'executions-list',
    status.recent_executions,
    e => `<div class="row">
      <div class="row-title">${exTag(e)}<span>${esc(e.action)}</span><span class="row-age">${ago(e.at)}</span></div>
      <div class="row-sub">${esc(e.preview)}</div>
    </div>`,
    'No executions yet'
  );

  // Analysis
  const analysis = document.getElementById('analysis-text');
  analysis.textContent = status.last_analysis || 'Waiting for first analysis cycle…';

  // Usage
  renderUsage(status.usage);

  // If the service rejected a settings change (e.g. a provider that can't
  // start), surface the reason next to the Save button — not just the header.
  if (status.error && /settings|not applied/i.test(status.error)) {
    const ss = document.getElementById('set-status');
    if (ss) ss.textContent = status.error;
  }
}

async function togglePause() {
  await invoke('toggle_pause');
  refresh();
}

// Approve/Reject a specific queued action. Buttons disable on click to avoid a
// double-submit; the card vanishes on the next status poll once the service has
// removed it from the queue.
async function decide(id, approved, card) {
  if (card) card.querySelectorAll('button').forEach(b => (b.disabled = true));
  try {
    await invoke('decide_approval', { id, approved });
  } catch (e) {
    console.error('decide_approval failed', e);
    if (card) card.querySelectorAll('button').forEach(b => (b.disabled = false));
  }
}

// ── Available app updates (winget + AI-driven) ──────────────────────────────

let updatesBusy = false;

// Escape a value for use inside a double-quoted HTML attribute.
function escAttr(s) { return esc(s).replace(/"/g, '&quot;'); }

// Find an update row (winget or AI) by its stable key.
function rowByKey(key) {
  return [...document.querySelectorAll('.upd-row')].find(r => r.dataset.key === key) || null;
}

// A coloured badge summarising one app's update outcome.
function outcomeBadge(o) {
  if (o.method === 'manual')         return '<span class="upd-badge tag-warn">Manual</span>';
  if (!o.success)                    return '<span class="upd-badge tag-block">Failed</span>';
  if (o.verification === 'verified') return '<span class="upd-badge tag-ok">Verified</span>';
  if (o.verification === 'mismatch') return '<span class="upd-badge tag-block">Not updated</span>';
  return '<span class="upd-badge tag-warn">Installed (unverified)</span>';
}

const PHASE_TEXT = {
  planning: 'finding installer…', downloading: 'downloading…',
  installing: 'installing…', verifying: 'verifying…', done: '', failed: '',
};

// Live per-row phase from the service's 'update-progress' events.
function applyPhase(key, phase) {
  if (!key || key === '*') return;
  const row = rowByKey(key);
  if (!row) return;
  const s = row.querySelector('.upd-status');
  if (s && !s.querySelector('.upd-badge')) s.textContent = PHASE_TEXT[phase] ?? phase;
}

// Render a finished AppOutcome onto its row: badge in the status slot, the Update
// button removed, and a one-line detail beneath.
function renderOutcome(o) {
  const row = rowByKey(o.key);
  if (!row) return;
  const actions = row.querySelector('.upd-actions');
  if (actions) actions.textContent = '';
  // Drop the AI row's Re-check/Note/Ignore buttons (siblings of .upd-actions) so
  // a later click can't replace the row and wipe this result.
  row.querySelectorAll('.upd-mini').forEach(b => b.remove());
  const status = row.querySelector('.upd-status');
  if (status) status.innerHTML = outcomeBadge(o);
  let res = row.querySelector('.upd-result');
  if (!res) { res = document.createElement('span'); res.className = 'upd-result'; row.appendChild(res); }
  const ver = o.to ? `now ${o.to}` : '';
  res.textContent = [o.detail, o.signature, ver].filter(Boolean).join(' · ');
}

function showSummary(outcomes) {
  const sum = document.getElementById('upd-summary');
  sum.style.display = 'block';
  sum.style.whiteSpace = 'pre-wrap';
  outcomes = outcomes || [];
  // Info/status rows (AI-check failure, truncation notes) carry a sentinel key
  // and are shown as notes, not counted as apps.
  const notes = outcomes.filter(o => o.key === '__info__');
  const apps = outcomes.filter(o => o.key !== '__info__');
  if (!apps.length && !notes.length) { sum.textContent = 'Nothing to update.'; return; }
  let verified = 0, unverified = 0, failed = 0, manual = 0;
  for (const o of apps) {
    if (o.method === 'manual') manual++;
    else if (!o.success) failed++;
    else if (o.verification === 'verified') verified++;
    else unverified++;
  }
  const head = [
    verified   && `${verified} verified`,
    unverified && `${unverified} installed (unverified)`,
    failed     && `${failed} failed`,
    manual     && `${manual} need manual install`,
  ].filter(Boolean).join(' · ') || (apps.length ? 'done' : '');
  const noteLines = notes.map(o => `• ${o.detail}`);
  const appLines = apps.map(o => {
    const tag = o.method === 'manual' ? 'manual'
      : !o.success ? 'failed'
      : o.verification === 'verified' ? 'verified' : 'unverified';
    return `${o.name}: ${tag}${o.detail ? ' — ' + o.detail : ''}`;
  });
  sum.textContent = [head, ...noteLines, ...appLines].filter(Boolean).join('\n');
}

async function loadUpdates() {
  const list = document.getElementById('updates-list');
  const countEl = document.getElementById('updates-count');
  const allBtn = document.getElementById('upd-all');
  // Don't refresh (and wipe result badges) while any update/install is running.
  if (updatesBusy || aiBusy) return;
  // Clear any stale "update everything" summary from a previous run.
  const sum = document.getElementById('upd-summary');
  if (sum) { sum.style.display = 'none'; sum.textContent = ''; }
  list.innerHTML = '<div class="empty">Checking for updates…</div>';
  try {
    const ups = await invoke('list_app_updates');
    countEl.textContent = ups.length ? `(${ups.length})` : '';
    allBtn.style.display = ups.length ? 'inline-block' : 'none';
    if (!ups.length) {
      list.innerHTML = '<div class="empty">All apps up to date</div>';
      return;
    }
    list.innerHTML = ups.map(u => `
      <div class="upd-row" data-key="${escAttr(u.id)}" data-id="${escAttr(u.id)}"
           data-name="${escAttr(u.name)}" data-current="${escAttr(u.current)}" data-available="${escAttr(u.available)}">
        <span class="upd-name" title="${escAttr(u.id)}">${esc(u.name)}</span>
        <span class="upd-ver">${esc(u.current)} → ${esc(u.available)}</span>
        <span class="upd-actions"><button class="upd-update">Update</button></span>
        <span class="upd-status"></span>
      </div>`).join('');
  } catch (e) {
    list.innerHTML = `<div class="empty">${esc(String(e))}</div>`;
    countEl.textContent = '';
    allBtn.style.display = 'none';
  }
}

async function updateOne(row, btn) {
  if (updatesBusy || aiBusy) return;
  updatesBusy = true;
  if (btn) { btn.disabled = true; btn.textContent = 'Updating…'; }
  try {
    const o = await invoke('update_app', {
      id: row.dataset.id, name: row.dataset.name,
      current: row.dataset.current, available: row.dataset.available,
    });
    renderOutcome(o);
  } catch (e) {
    if (btn) { btn.disabled = false; btn.textContent = 'Failed'; btn.title = String(e); }
    console.error('update_app failed', e);
  }
  updatesBusy = false;
}

async function updateAll() {
  if (updatesBusy || aiBusy) return;
  updatesBusy = true;
  const btn = document.getElementById('upd-all');
  btn.disabled = true; btn.textContent = 'Updating…';
  try {
    const outcomes = await invoke('update_all_apps');
    outcomes.forEach(renderOutcome);
    showSummary(outcomes);
  } catch (e) {
    console.error('update_all_apps failed', e);
    btn.title = String(e);
  }
  btn.disabled = false; btn.textContent = 'Update all (winget)';
  updatesBusy = false;
}

// Update EVERYTHING: winget apps (one UAC) then AI-driven installs of non-winget
// apps (downloaded, then one UAC), each verified. Progress streams in live.
async function updateEverything() {
  if (updatesBusy || aiBusy) return;
  updatesBusy = true; aiBusy = true;
  const btn = document.getElementById('upd-everything');
  btn.disabled = true; btn.textContent = 'Updating everything…';
  const sum = document.getElementById('upd-summary');
  sum.style.display = 'block'; sum.style.whiteSpace = 'pre-wrap';
  sum.textContent = 'Updating winget apps, then finding and installing other apps… this can take a few minutes.';
  try {
    const outcomes = await invoke('update_everything');
    outcomes.forEach(renderOutcome);   // updates any visible rows
    showSummary(outcomes);             // full per-app report
  } catch (e) {
    console.error('update_everything failed', e);
    sum.textContent = 'Update everything failed: ' + String(e);
  }
  btn.disabled = false; btn.textContent = '⬆ Update everything';
  updatesBusy = false; aiBusy = false;
}

// Install one non-winget app via the AI (plan → download → install → verify).
async function installAiApp(row, btn) {
  if (updatesBusy || aiBusy) return;
  aiBusy = true;
  if (btn) { btn.disabled = true; btn.textContent = 'Installing…'; }
  try {
    const o = await invoke('install_ai_app', {
      name: row.dataset.name, current: row.dataset.current || '',
    });
    renderOutcome(o);
  } catch (e) {
    if (btn) { btn.disabled = false; btn.textContent = 'Failed'; btn.title = String(e); }
    console.error('install_ai_app failed', e);
  }
  aiBusy = false;
}

// Header / approval buttons (wired here rather than inline — Tauri v2 injects a
// CSP nonce that disables 'unsafe-inline', which would block inline onclick).
document.getElementById('pause-btn').addEventListener('click', togglePause);

// Approve/Reject buttons are rendered per-card, so delegate from the container.
document.getElementById('approvals').addEventListener('click', (e) => {
  const btn = e.target.closest('.btn-approve, .btn-reject');
  if (!btn) return;
  const id = parseInt(btn.dataset.id, 10);
  if (!Number.isFinite(id)) return;
  decide(id, btn.classList.contains('btn-approve'), btn.closest('.approval-card'));
});

document.getElementById('upd-refresh').addEventListener('click', loadUpdates);
document.getElementById('upd-all').addEventListener('click', updateAll);
document.getElementById('upd-everything').addEventListener('click', updateEverything);
document.getElementById('updates-list').addEventListener('click', (e) => {
  const b = e.target.closest('.upd-update');
  if (b) updateOne(b.closest('.upd-row'), b);
});

// Live per-row progress for any update/install in flight (no polling).
window.__TAURI__.event.listen('update-progress', (e) => {
  const p = e.payload || {};
  applyPhase(p.key, p.phase);
}).catch(err => console.error('progress subscribe failed', err));

// ── Other updates (AI-checked) ──────────────────────────────────────────────

let aiBusy = false;

async function checkAiUpdates() {
  if (aiBusy) return;
  aiBusy = true;
  const list = document.getElementById('ai-updates-list');
  const btn = document.getElementById('ai-check-btn');
  btn.disabled = true; btn.textContent = 'Checking…';
  list.innerHTML = '<div class="empty">Asking AI to check the web for newer versions… this can take a minute.</div>';
  try {
    const r = await invoke('check_ai_updates');
    const cost = `Checked ${r.checked} app${r.checked === 1 ? '' : 's'} · ~${fmtGbp(r.cost_usd)}`;
    let html = '<div class="upd-note">Apps not covered by winget:</div>';
    if (r.note) html += `<div class="upd-note">${esc(r.note)}</div>`;
    if (!r.updates.length) {
      html += '<div class="empty">No updates found for non-winget apps.</div>';
    } else {
      html += r.updates.map(u => `
        <div class="upd-row" data-key="${escAttr((u.name || '').toLowerCase())}" data-name="${escAttr(u.name)}" data-current="${escAttr(u.current || '')}">
          <span class="upd-name" title="${escAttr(u.name)}">${esc(u.name)}</span>
          <span class="upd-ver">${esc(u.current || '?')} → ${esc(u.latest)}</span>
          <span class="upd-actions"><button class="upd-install install-btn">Install</button>${u.url ? `<button class="upd-dl" data-url="${escAttr(u.url)}">Download</button>` : ''}</span>
          <button class="upd-mini recheck-btn">Re-check</button>
          <button class="upd-mini note-btn">Note</button>
          <button class="upd-mini ignore-btn">Ignore</button>
          <span class="upd-status"></span>
        </div>`).join('');
    }
    html += `<div class="upd-note">${esc(cost)} · AI installs are downloaded, verified, and version-checked. Verify before installing.</div>`;
    list.innerHTML = html;
  } catch (e) {
    list.innerHTML = `<div class="empty">${esc(String(e))}</div>`;
  }
  aiBusy = false;
  btn.disabled = false; btn.textContent = 'Check other apps';
}

document.getElementById('ai-check-btn').addEventListener('click', checkAiUpdates);
document.getElementById('ai-updates-list').addEventListener('click', (e) => {
  // Download link
  const dl = e.target.closest('.upd-dl');
  if (dl) { invoke('open_url', { url: dl.dataset.url }).catch(err => console.error(err)); return; }
  const row = e.target.closest('.upd-row');
  if (!row) return;
  const name = row.dataset.name;
  // AI-driven install of this app (plan → download → install → verify)
  const inst = e.target.closest('.install-btn');
  if (inst) { installAiApp(row, inst); return; }
  // Ignore: blacklist this app from future AI checks
  if (e.target.closest('.ignore-btn')) {
    invoke('set_app_note', { name, note: '', ignore: true })
      .then(() => { row.innerHTML =
        `<span class="upd-name">${esc(name)}</span><span class="upd-ver">ignored — won't be checked again</span>`; })
      .catch(err => console.error(err));
    return;
  }
  // Note: open an inline editor for a hint used in future checks
  if (e.target.closest('.note-btn')) {
    row.innerHTML = `
      <span class="upd-name">${esc(name)}</span>
      <span class="note-edit">
        <input class="note-input" type="text" placeholder="e.g. my own app — ignore / latest is on my GitHub releases">
        <button class="upd-mini note-save">Save</button>
      </span>`;
    const inp = row.querySelector('.note-input');
    inp.focus();
    inp.addEventListener('keydown', ev => { if (ev.key === 'Enter') row.querySelector('.note-save').click(); });
    return;
  }
  // Save the typed note, then offer a one-app re-check using it
  if (e.target.closest('.note-save')) {
    const val = row.querySelector('.note-input').value;
    invoke('set_app_note', { name, note: val, ignore: false })
      .then(() => { row.innerHTML =
        `<span class="upd-name">${esc(name)}</span><span class="upd-ver">note saved</span>` +
        `<button class="upd-mini recheck-btn">Re-check now</button>`; })
      .catch(err => console.error(err));
    return;
  }
  // Re-check just this app (uses its stored note) — no full sweep
  if (e.target.closest('.recheck-btn')) {
    const current = row.dataset.current || '';
    row.innerHTML = `<span class="upd-name">${esc(name)}</span><span class="upd-ver">re-checking…</span>`;
    invoke('check_app_update', { name, current }).then(r => {
      if (r.updates && r.updates.length) {
        const u = r.updates[0];
        // Keep the row's own dataset current so a later Install sends the right version.
        row.dataset.current = u.current || current || '';
        row.innerHTML =
          `<span class="upd-name" title="${escAttr(name)}">${esc(name)}</span>` +
          `<span class="upd-ver">${esc(u.current || current || '?')} → ${esc(u.latest)} · ~${fmtGbp(r.cost_usd)}</span>` +
          `<span class="upd-actions"><button class="upd-install install-btn">Install</button>` +
          (u.url ? `<button class="upd-dl" data-url="${escAttr(u.url)}">Download</button>` : '') + `</span>` +
          `<button class="upd-mini recheck-btn">Re-check</button>` +
          `<button class="upd-mini note-btn">Note</button>` +
          `<button class="upd-mini ignore-btn">Ignore</button>` +
          `<span class="upd-status"></span>`;
      } else {
        row.innerHTML =
          `<span class="upd-name">${esc(name)}</span>` +
          `<span class="upd-ver">up to date${r.note ? ' — ' + esc(r.note) : ''} · ~${fmtGbp(r.cost_usd)}</span>` +
          `<button class="upd-mini note-btn">Note</button>`;
      }
    }).catch(err => {
      row.innerHTML = `<span class="upd-name">${esc(name)}</span><span class="upd-ver">re-check failed: ${esc(String(err))}</span><button class="upd-mini recheck-btn">Re-check</button>`;
    });
    return;
  }
});

// ── Settings ────────────────────────────────────────────────────────────────

function fillSettings() {
  const s = lastStatus && lastStatus.settings;
  if (!s) return;
  document.getElementById('set-provider').value = s.provider || 'openrouter';
  document.getElementById('set-model').value = s.model || '';
  document.getElementById('set-effort').value = s.effort || '';
  document.getElementById('set-upd-model').value = s.update_check_model || '';
  document.getElementById('set-base').value = s.base_url || '';
  document.getElementById('set-conf').value = Math.round((s.confidence_threshold || 0.80) * 100);
  document.getElementById('set-decint').value = s.decision_interval_secs || 600;
  document.getElementById('set-elpoll').value = s.event_log_poll_interval_secs || 30;
  document.getElementById('set-wmipoll').value = s.wmi_poll_interval_secs || 300;
  document.getElementById('set-channels').value = (s.event_log_channels || []).join(', ');
  document.getElementById('set-dirs').value = (s.log_directories || []).join(', ');
  document.getElementById('set-or-key').placeholder =
    s.openrouter_key_set ? '•••••• set — blank keeps it' : 'not set';
  document.getElementById('set-an-key').placeholder =
    s.anthropic_key_set ? '•••••• set — blank keeps it' : 'not set';
}

function toggleSettings() {
  const body = document.getElementById('settings-body');
  const showing = body.style.display !== 'none';
  if (showing) {
    body.style.display = 'none';
    document.getElementById('settings-show').textContent = 'Show';
  } else {
    fillSettings();
    body.style.display = 'block';
    document.getElementById('settings-show').textContent = 'Hide';
  }
}

async function saveSettings() {
  const splitList = v => v.split(/[,\n]/).map(x => x.trim()).filter(Boolean);
  const orKey = document.getElementById('set-or-key').value.trim();
  const anKey = document.getElementById('set-an-key').value.trim();
  const settings = {
    provider: document.getElementById('set-provider').value,
    model: document.getElementById('set-model').value.trim(),
    effort: document.getElementById('set-effort').value,
    update_check_model: document.getElementById('set-upd-model').value.trim(),
    base_url: document.getElementById('set-base').value.trim(),
    openrouter_api_key: orKey || null,
    anthropic_api_key: anKey || null,
    api_key: null,
    confidence_threshold: (parseInt(document.getElementById('set-conf').value, 10) || 80) / 100,
    decision_interval_secs: parseInt(document.getElementById('set-decint').value, 10) || 600,
    event_log_poll_interval_secs: parseInt(document.getElementById('set-elpoll').value, 10) || 30,
    wmi_poll_interval_secs: parseInt(document.getElementById('set-wmipoll').value, 10) || 300,
    event_log_channels: splitList(document.getElementById('set-channels').value),
    log_directories: splitList(document.getElementById('set-dirs').value),
  };
  const st = document.getElementById('set-status');

  // Client-side guard: the service rejects (and silently reverts) a provider
  // that can't start. Catch the common cases here with a clear message instead
  // of a confusing revert to the previous provider.
  const s = (lastStatus && lastStatus.settings) || {};
  // OpenRouter: a blank model means the free auto-routing model. Don't hard-block
  // on a missing key — the service can auto-detect one from the OpenRouter CLI
  // (~/.openrouter/config.json) and will report back if none is found anywhere.
  if (settings.provider === 'openrouter' && !settings.model) {
    settings.model = 'openrouter/free';
  }
  if (settings.provider === 'anthropic') {
    if (!anKey && !s.anthropic_key_set) {
      st.textContent = 'Anthropic needs an API key — enter one above, then Save.';
      return;
    }
    if (!settings.model) {
      st.textContent = 'Anthropic needs a model — e.g. claude-haiku-4-5';
      return;
    }
  }

  st.textContent = 'Saving… the service will restart (~15s).';
  try {
    await invoke('update_settings', { settings });
    st.textContent = 'Saved. Service restarting — it will reconnect shortly.';
    document.getElementById('set-or-key').value = '';
    document.getElementById('set-an-key').value = '';
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  }
}

document.getElementById('settings-toggle').addEventListener('click', toggleSettings);
document.getElementById('set-save').addEventListener('click', saveSettings);

// Clear the in-memory Recents (service-side; the next poll reflects the empty list).
document.getElementById('clear-problems').addEventListener('click', async () => {
  try { await invoke('clear_problems'); } catch (e) { console.error(e); }
  refresh();
});
document.getElementById('clear-executions').addEventListener('click', async () => {
  try { await invoke('clear_executions'); } catch (e) { console.error(e); }
  refresh();
});

// ── Collapsible cards ───────────────────────────────────────────────────────

function loadCollapsed() {
  try { return JSON.parse(localStorage.getItem('eirCollapsed') || '{}'); }
  catch { return {}; }
}

function applyCollapsed() {
  const saved = loadCollapsed();
  Object.entries(saved).forEach(([id, collapsed]) => {
    const el = document.getElementById(id);
    if (el && collapsed) el.classList.add('collapsed');
  });
}

document.querySelector('.body').addEventListener('click', (e) => {
  // Don't toggle when interacting with a control inside the header.
  if (e.target.closest('button, input, select, a')) return;
  const header = e.target.closest('.card-header.collapsible');
  if (!header) return;
  const card = header.closest('.card, .analysis-card');
  if (!card || !card.id) return;
  card.classList.toggle('collapsed');
  const saved = loadCollapsed();
  saved[card.id] = card.classList.contains('collapsed');
  localStorage.setItem('eirCollapsed', JSON.stringify(saved));
});

applyCollapsed();

// Initial load + poll every 2 seconds
refresh();
setInterval(refresh, 2000);

// Check for app updates on launch, then hourly
loadUpdates();
setInterval(loadUpdates, 60 * 60 * 1000);

// Fetch the USD→GBP rate so costs display in pounds
invoke('gbp_per_usd').then(r => { if (r > 0) gbpRate = r; }).catch(() => {});
