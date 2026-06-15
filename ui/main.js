const invoke = window.__TAURI__.core.invoke;

// The app-update check needs live web search, so it always runs on a Claude
// model. Mirror the backend: blank or a non-Claude id resolves to "haiku".
function effectiveUpdateModel(model) {
  const m = (model || '').trim();
  const lower = m.toLowerCase();
  const isClaude = ['haiku', 'sonnet', 'opus'].includes(lower) || lower.startsWith('claude');
  return isClaude ? m : 'haiku';
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

function renderApproval(info) {
  const card = document.getElementById('approval-card');
  if (!info) { card.style.display = 'none'; return; }

  card.dataset.approvalId = info.id;
  card.style.display = 'block';
  document.getElementById('approval-grid').innerHTML = `
    <span class="label">Diagnosis</span>  <span class="val">${esc(info.diagnosis)}</span>
    <span class="label">Root cause</span> <span class="val">${esc(info.root_cause)}</span>
    <span class="label">Confidence</span> <span class="val">${Math.round(info.confidence * 100)}%</span>
    <span class="label">Action</span>     <span class="val">${esc(info.action)}</span>
    <span class="label">Reason</span>     <span class="val">${esc(info.reason)}</span>
    <span class="label">Side effects</span><span class="val">${esc(info.side_effects)}</span>
    <span class="label">Undo</span>       <span class="val">${esc(info.undo_instructions)}</span>
  `;
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
    ml.title = `Updates check uses: ${effectiveUpdateModel(s.update_check_model)}`;
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

  // Approval card
  renderApproval(status.pending_approval);

  // Problems
  renderList(
    'problems-list',
    status.recent_problems,
    p => `<div class="row">
      <div class="row-title">${problemTag(p)}<span>${esc(p.diagnosis)}</span></div>
      <div class="row-sub">${esc(p.action)}</div>
    </div>`,
    'No problems detected yet'
  );

  // Executions
  renderList(
    'executions-list',
    status.recent_executions,
    e => `<div class="row">
      <div class="row-title">${exTag(e)}<span>${esc(e.action)}</span></div>
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

async function decide(approved) {
  const card = document.getElementById('approval-card');
  const id = parseInt(card.dataset.approvalId ?? '0', 10);
  await invoke('decide_approval', { id, approved });
  card.style.display = 'none';
}

// ── Available app updates (winget) ──────────────────────────────────────────

let updatesBusy = false;

async function loadUpdates() {
  const list = document.getElementById('updates-list');
  const countEl = document.getElementById('updates-count');
  const allBtn = document.getElementById('upd-all');
  if (updatesBusy) return;
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
      <div class="upd-row">
        <span class="upd-name" title="${esc(u.id)}">${esc(u.name)}</span>
        <span class="upd-ver">${esc(u.current)} → ${esc(u.available)}</span>
        <button class="upd-update" data-id="${esc(u.id)}">Update</button>
      </div>`).join('');
  } catch (e) {
    list.innerHTML = `<div class="empty">${esc(String(e))}</div>`;
    countEl.textContent = '';
    allBtn.style.display = 'none';
  }
}

async function updateOne(id, btn) {
  if (updatesBusy) return;
  updatesBusy = true;
  if (btn) { btn.disabled = true; btn.textContent = 'Updating…'; }
  try {
    await invoke('update_app', { id });
  } catch (e) {
    if (btn) btn.textContent = 'Failed';
    console.error('update_app failed', e);
    updatesBusy = false;
    return;
  }
  updatesBusy = false;
  loadUpdates();
}

async function updateAll() {
  if (updatesBusy) return;
  updatesBusy = true;
  const btn = document.getElementById('upd-all');
  btn.disabled = true; btn.textContent = 'Updating…';
  try {
    await invoke('update_all_apps');
  } catch (e) {
    console.error('update_all_apps failed', e);
  }
  updatesBusy = false;
  btn.disabled = false; btn.textContent = 'Update all';
  loadUpdates();
}

// Header / approval buttons (wired here rather than inline — Tauri v2 injects a
// CSP nonce that disables 'unsafe-inline', which would block inline onclick).
document.getElementById('pause-btn').addEventListener('click', togglePause);
document.getElementById('approve-btn').addEventListener('click', () => decide(true));
document.getElementById('reject-btn').addEventListener('click', () => decide(false));

document.getElementById('upd-refresh').addEventListener('click', loadUpdates);
document.getElementById('upd-all').addEventListener('click', updateAll);
document.getElementById('updates-list').addEventListener('click', (e) => {
  const b = e.target.closest('.upd-update');
  if (b) updateOne(b.dataset.id, b);
});

// ── Other updates (AI-checked) ──────────────────────────────────────────────

let aiBusy = false;

async function checkAiUpdates() {
  if (aiBusy) return;
  aiBusy = true;
  const list = document.getElementById('ai-updates-list');
  const countEl = document.getElementById('ai-updates-count');
  const btn = document.getElementById('ai-check-btn');
  btn.disabled = true; btn.textContent = 'Checking…';
  list.innerHTML = '<div class="empty">Asking AI to check the web for newer versions… this can take a minute.</div>';
  try {
    const model = effectiveUpdateModel(lastStatus && lastStatus.settings && lastStatus.settings.update_check_model);
    const r = await invoke('check_ai_updates', { model });
    const cost = `Checked ${r.checked} app${r.checked === 1 ? '' : 's'} · ~${fmtGbp(r.cost_usd)}`;
    countEl.textContent = r.updates.length ? `(${r.updates.length})` : '';
    let html = '';
    if (r.note) html += `<div class="upd-note">${esc(r.note)}</div>`;
    if (!r.updates.length) {
      html += '<div class="empty">No updates found for non-winget apps.</div>';
    } else {
      html += r.updates.map(u => `
        <div class="upd-row" data-name="${esc(u.name)}" data-current="${esc(u.current || '')}">
          <span class="upd-name" title="${esc(u.name)}">${esc(u.name)}</span>
          <span class="upd-ver">${esc(u.current || '?')} → ${esc(u.latest)}</span>
          ${u.url ? `<button class="upd-dl" data-url="${esc(u.url)}">Download</button>` : ''}
          <button class="upd-mini recheck-btn">Re-check</button>
          <button class="upd-mini note-btn">Note</button>
          <button class="upd-mini ignore-btn">Ignore</button>
        </div>`).join('');
    }
    html += `<div class="upd-note">${esc(cost)} · AI-checked — verify before installing.</div>`;
    list.innerHTML = html;
  } catch (e) {
    list.innerHTML = `<div class="empty">${esc(String(e))}</div>`;
    countEl.textContent = '';
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
    const model = effectiveUpdateModel(lastStatus && lastStatus.settings && lastStatus.settings.update_check_model);
    row.innerHTML = `<span class="upd-name">${esc(name)}</span><span class="upd-ver">re-checking…</span>`;
    invoke('check_app_update', { name, current, model }).then(r => {
      if (r.updates && r.updates.length) {
        const u = r.updates[0];
        row.innerHTML =
          `<span class="upd-name" title="${esc(name)}">${esc(name)}</span>` +
          `<span class="upd-ver">${esc(u.current || current || '?')} → ${esc(u.latest)} · ~${fmtGbp(r.cost_usd)}</span>` +
          (u.url ? `<button class="upd-dl" data-url="${esc(u.url)}">Download</button>` : '') +
          `<button class="upd-mini recheck-btn">Re-check</button>` +
          `<button class="upd-mini note-btn">Note</button>` +
          `<button class="upd-mini ignore-btn">Ignore</button>`;
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
  document.getElementById('set-upd-model').value = s.update_check_model || '';
  document.getElementById('set-base').value = s.base_url || '';
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
    update_check_model: document.getElementById('set-upd-model').value.trim(),
    base_url: document.getElementById('set-base').value.trim(),
    openrouter_api_key: orKey || null,
    anthropic_api_key: anKey || null,
    api_key: null,
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
  if (settings.provider === 'openrouter') {
    if (!orKey && !s.openrouter_key_set) {
      st.textContent = 'OpenRouter needs an API key — enter one above, then Save.';
      return;
    }
    if (!settings.model) {
      st.textContent = 'OpenRouter needs a model — e.g. nvidia/nemotron-3-super-120b-a12b:free';
      return;
    }
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
