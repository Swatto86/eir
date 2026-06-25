const invoke = window.__TAURI__.core.invoke;

function providerName(p) {
  return ({
    openrouter: 'OpenRouter',
    claude_cli: 'Claude CLI',
    anthropic: 'Anthropic',
    openai_compatible: 'OpenAI-compatible',
  })[p] || p || '';
}

// Model (and Claude-CLI reasoning effort) used for general issue analysis — the
// main monitoring/diagnosis loop.
function analysisLabel(s) {
  if (!s) return '';
  let model = (s.model || '').trim();
  if (!model) {
    model = s.provider === 'openrouter' ? 'openrouter/free'
      : s.provider === 'claude_cli' ? 'default model'
      : '(no model set)';
  }
  let label = `${providerName(s.provider)} · ${model}`;
  const effort = (s.effort || '').trim();
  if (s.provider === 'claude_cli' && effort) label += ` · ${effort} effort`;
  return label;
}

// Which provider/model the app-update web check uses.
function updateCheckLabel(s) {
  if (!s) return '';
  if (s.provider === 'openrouter') {
    const m = (s.model || '').trim() || 'openrouter/free';
    return `OpenRouter · ${m} + web`;
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

// Relative time until a future unix-seconds timestamp.
function until(ts) {
  if (!ts) return '';
  const s = Math.floor(ts - Date.now() / 1000);
  if (s <= 0) return 'due';
  if (s < 3600) return `${Math.ceil(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86400)}d`;
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

// Build one approval card.
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

// Escape a value for use inside a double-quoted HTML attribute.
function escAttr(s) { return esc(s).replace(/"/g, '&quot;'); }

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

  const dot = document.getElementById('status-dot');
  const txt = document.getElementById('status-text');
  dot.style.background = STATUS_COLORS[status.status] ?? 'var(--gray)';
  txt.textContent = status.error
    ? `Error: ${status.error}`
    : status.status.replace(/([A-Z])/g, ' $1').trim();

  const ml = document.getElementById('model-label');
  if (status.settings) {
    const s = status.settings;
    const analysis = analysisLabel(s);
    const updates = updateCheckLabel(s);
    ml.innerHTML =
      `<span class="ml-line"><span class="ml-key">Analysis</span>${esc(analysis)}</span>` +
      `<span class="ml-line"><span class="ml-key">Updates</span>${esc(updates)}</span>`;
    ml.title = `Issue analysis: ${analysis}\nApp-update check: ${updates}`;
  } else {
    ml.textContent = '';
  }

  document.getElementById('pause-btn').textContent =
    status.paused ? 'Resume' : 'Pause';

  document.getElementById('cpu').textContent    = pct(status.cpu);
  document.getElementById('memory').textContent = pct(status.memory);
  document.getElementById('disk').textContent   = pct(status.disk);
  setBar('cpu-bar',  status.cpu);
  setBar('mem-bar',  status.memory);
  setBar('disk-bar', status.disk);

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

  renderApprovals(status.pending_approvals);

  // What the agent is thinking + the chronological activity feed.
  renderAiNow(status);
  renderActivity(status);

  renderUsage(status.usage);

  // App updates (service-driven).
  renderUpdater(status.updater);

  if (status.error && /settings|not applied/i.test(status.error)) {
    const ss = document.getElementById('set-status');
    if (ss) ss.textContent = status.error;
  }
}

async function togglePause() {
  await invoke('toggle_pause');
  refresh();
}

// Approve/Reject a specific queued action.
async function decide(id, approved, card) {
  if (card) card.querySelectorAll('button').forEach(b => (b.disabled = true));
  try {
    await invoke('decide_approval', { id, approved });
  } catch (e) {
    console.error('decide_approval failed', e);
    if (card) card.querySelectorAll('button').forEach(b => (b.disabled = false));
  }
}

// ── What the agent is thinking + the activity feed ──────────────────────────

function renderAiNow(status) {
  const txt = document.getElementById('ai-now-text');
  const meta = document.getElementById('ai-now-meta');
  txt.textContent = status.last_analysis || 'Waiting for the first analysis cycle…';
  const bits = [];
  if (status.settings) bits.push(`<span>${esc(analysisLabel(status.settings))}</span>`);
  const a = status.advisor;
  if (a && a.escalated) {
    bits.push(`<span class="ai-badge ai-escalated">⤴ escalated${a.escalation_model ? ' → ' + esc(a.escalation_model) : ''}</span>`);
    if (a.reason) bits.push(`<span>${esc(a.reason)}</span>`);
  } else if (a && a.enabled) {
    bits.push('<span class="ai-badge tag-ok">advisor on</span>');
  }
  if (a && a.spent_today_usd) bits.push(`<span>escalation spend today ~${fmtGbp(a.spent_today_usd)}</span>`);
  meta.innerHTML = bits.join('');
}

// Merge problems (diagnoses) + executions (fixes) into one chronological list.
function activityItems(status) {
  const items = [];
  for (const p of (status.recent_problems || [])) {
    const icon = p.blocked ? '🚫' : (p.auto_executed ? '🔧' : '🔎');
    const why = [p.action, p.reason].filter(Boolean).map(esc).join(' — ');
    items.push({ at: p.at || 0, icon, head: `${problemTag(p)}<span class="act-text" title="${escAttr(p.diagnosis)}">${esc(p.diagnosis)}</span>`, why });
  }
  for (const e of (status.recent_executions || [])) {
    const icon = e.success ? '✅' : '❌';
    items.push({ at: e.at || 0, icon, head: `${exTag(e)}<span class="act-text" title="${escAttr(e.action)}">${esc(e.action)}</span>`, why: esc(e.preview || '') });
  }
  items.sort((a, b) => (b.at || 0) - (a.at || 0));
  return items;
}

function renderActivity(status) {
  const el = document.getElementById('activity-list');
  const items = activityItems(status);
  if (!items.length) { el.innerHTML = '<div class="empty">No activity yet</div>'; return; }
  el.innerHTML = items.map(it => `
    <div class="act-item">
      <div class="act-icon">${it.icon}</div>
      <div class="act-main">
        <div class="act-head">${it.head}<span class="act-when">${ago(it.at)}</span></div>
        ${it.why ? `<div class="act-why">${it.why}</div>` : ''}
      </div>
    </div>`).join('');
}

// ── App updates (driven by the LocalSystem service — no UAC) ─────────────────

const UPD_BADGE = {
  verified:  '<span class="upd-badge tag-ok">Verified</span>',
  installed: '<span class="upd-badge tag-warn">Installed</span>',
  failed:    '<span class="upd-badge tag-block">Failed</span>',
  skipped:   '<span class="upd-badge tag-warn">Skipped</span>',
};

function methodLabel(m) {
  return ({ winget: 'winget', choco: 'Chocolatey', scoop: 'Scoop', msstore: 'Store', native: 'AI installer' })[m] || m || '';
}

function updaterAppRow(a) {
  const ver = `${esc(a.from || '?')}${a.to ? ' → ' + esc(a.to) : ''}`;
  const badge = UPD_BADGE[a.state] || '';
  const meth = a.method ? `<span class="upd-status">via ${esc(methodLabel(a.method))}</span>` : '';
  const detailText = [a.detail, a.signature].filter(Boolean).join(' · ');
  const detail = detailText ? `<span class="upd-result">${esc(detailText)}</span>` : '';
  return `<div class="upd-row" data-id="${escAttr(a.id)}">
    <span class="upd-name" title="${escAttr(a.name)}">${esc(a.name)}</span>
    <span class="upd-ver">${ver}</span>${meth}${badge}
    <button class="upd-mini upd-ignore" data-id="${escAttr(a.id)}" title="Don't check this app again">Ignore</button>
    ${detail}
  </div>`;
}

function renderUpdater(u) {
  const stateEl = document.getElementById('updater-state');
  const metaEl = document.getElementById('updater-meta');
  const appsEl = document.getElementById('updater-apps');
  const notesEl = document.getElementById('updater-notes');
  const histWrap = document.getElementById('updater-history-wrap');
  const histEl = document.getElementById('updater-history');
  const nowBtn = document.getElementById('upd-now');
  if (!u) { stateEl.textContent = ''; return; }

  stateEl.textContent = u.running ? '· running…' : (u.enabled ? '· auto' : '· off');
  if (nowBtn) {
    // The service ignores a manual run unless the updater is enabled (the master
    // switch also gates the pipe-triggered run), so reflect that in the button.
    nowBtn.disabled = !!u.running || !u.enabled;
    nowBtn.textContent = u.running ? 'Updating…' : '⬆ Update now';
    nowBtn.title = u.enabled ? '' : 'Enable auto-updates in Settings first';
  }

  const bits = [];
  if (u.running && u.phase) bits.push(esc(u.phase));
  if (u.last_run) bits.push('last run ' + ago(u.last_run));
  if (u.enabled && u.next_run) bits.push('next in ' + until(u.next_run));
  if (u.last_cost_usd) bits.push('~' + fmtGbp(u.last_cost_usd));
  metaEl.style.display = bits.length ? 'block' : 'none';
  metaEl.textContent = bits.join(' · ');

  if (u.apps && u.apps.length) {
    appsEl.innerHTML = u.apps.map(updaterAppRow).join('');
  } else if (u.running) {
    appsEl.innerHTML = '<div class="empty">Checking for updates…</div>';
  } else if (u.last_run) {
    appsEl.innerHTML = '<div class="empty">Everything up to date.</div>';
  } else {
    appsEl.innerHTML = '<div class="empty">Enable auto-updates in Settings, or click “Update now”.</div>';
  }

  notesEl.innerHTML = (u.notes && u.notes.length)
    ? u.notes.map(n => `<div class="upd-note">• ${esc(n)}</div>`).join('') : '';

  if (u.recent && u.recent.length) {
    histWrap.style.display = 'block';
    histEl.innerHTML = u.recent.slice(0, 15).map(r =>
      `<div class="upd-note">${r.success ? '✓' : '✗'} ${esc(r.name)} ` +
      `<span style="opacity:.7">(${esc(methodLabel(r.method))})</span>` +
      `${r.detail ? ' — ' + esc(r.detail) : ''} <span class="row-age">${ago(r.at)}</span></div>`
    ).join('');
  } else {
    histWrap.style.display = 'none';
  }
}

async function updateNow() {
  try { await invoke('run_updates_now'); }
  catch (e) { console.error('run_updates_now failed', e); }
}

// ── Updater settings (apply live — no service restart) ───────────────────────

const METHOD_BOXES = [['m-winget', 'winget'], ['m-choco', 'choco'], ['m-scoop', 'scoop'], ['m-msstore', 'msstore']];

function fillUpdaterSettings(s) {
  if (!s) return;
  document.getElementById('set-upd-enabled').checked = !!s.enabled;
  document.getElementById('set-upd-interval').value =
    Math.max(1, Math.round((s.schedule_interval_secs || 86400) / 3600));
  const methods = s.methods || [];
  for (const [id, name] of METHOD_BOXES) document.getElementById(id).checked = methods.includes(name);
  document.getElementById('set-native-enabled').checked = !!s.native_enabled;
  document.getElementById('set-sigpol').value = s.native_signature_policy || 'require_valid';
}

async function saveUpdaterSettings() {
  const methods = METHOD_BOXES.filter(([id]) => document.getElementById(id).checked).map(([, n]) => n);
  const settings = {
    enabled: document.getElementById('set-upd-enabled').checked,
    schedule_interval_secs:
      Math.max(1, parseInt(document.getElementById('set-upd-interval').value, 10) || 24) * 3600,
    methods,
    native_enabled: document.getElementById('set-native-enabled').checked,
    native_signature_policy: document.getElementById('set-sigpol').value,
  };
  const st = document.getElementById('set-upd-status');
  st.textContent = 'Saving…';
  try {
    await invoke('set_updater_settings', { settings });
    st.textContent = 'Saved — applies immediately.';
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  }
}

// ── Header / approval / update wiring ────────────────────────────────────────
// (wired here rather than inline — Tauri v2's CSP nonce blocks inline handlers)
document.getElementById('pause-btn').addEventListener('click', togglePause);

document.getElementById('approvals').addEventListener('click', (e) => {
  const btn = e.target.closest('.btn-approve, .btn-reject');
  if (!btn) return;
  const id = parseInt(btn.dataset.id, 10);
  if (!Number.isFinite(id)) return;
  decide(id, btn.classList.contains('btn-approve'), btn.closest('.approval-card'));
});

document.getElementById('upd-now').addEventListener('click', updateNow);
document.getElementById('set-upd-save').addEventListener('click', saveUpdaterSettings);

// Per-app "Ignore" — stop checking this app (delegated from the list).
document.getElementById('updater-apps').addEventListener('click', (e) => {
  const ig = e.target.closest('.upd-ignore');
  if (!ig) return;
  invoke('set_app_ignore', { id: ig.dataset.id, ignore: true, note: '' })
    .then(() => { const row = ig.closest('.upd-row'); if (row) row.style.opacity = '.5'; })
    .catch(err => console.error('set_app_ignore failed', err));
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
  fillUpdaterSettings(lastStatus.updater && lastStatus.updater.settings);
  fillAdvisorSettings(lastStatus.advisor && lastStatus.advisor.settings);
}

function openSettings() {
  fillSettings();
  document.getElementById('settings-modal').classList.add('open');
}
function closeSettings() {
  document.getElementById('settings-modal').classList.remove('open');
}

// ── Advisor settings (apply live — no service restart) ───────────────────────

function fillAdvisorSettings(s) {
  if (!s) return;
  document.getElementById('set-adv-enabled').checked = !!s.enabled;
  document.getElementById('set-adv-model').value = s.escalation_model || '';
  document.getElementById('set-adv-effort').value = s.escalation_effort || '';
  document.getElementById('set-adv-conf').value = Math.round((s.low_confidence_threshold || 0.6) * 100);
  document.getElementById('set-adv-budget').value =
    s.budget_usd_per_day != null ? s.budget_usd_per_day : 0.5;
}

async function saveAdvisorSettings() {
  const settings = {
    enabled: document.getElementById('set-adv-enabled').checked,
    escalation_model: document.getElementById('set-adv-model').value.trim(),
    escalation_effort: document.getElementById('set-adv-effort').value,
    low_confidence_threshold: (parseInt(document.getElementById('set-adv-conf').value, 10) || 60) / 100,
    budget_usd_per_day: parseFloat(document.getElementById('set-adv-budget').value) || 0,
  };
  const st = document.getElementById('set-adv-status');
  st.textContent = 'Saving…';
  try {
    await invoke('set_advisor_settings', { settings });
    st.textContent = 'Saved — applies immediately.';
  } catch (e) {
    st.textContent = 'Failed: ' + e;
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

  const s = (lastStatus && lastStatus.settings) || {};
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

// Settings modal: open from the gear; close via ×, backdrop click, or Escape.
document.getElementById('open-settings').addEventListener('click', openSettings);
document.getElementById('close-settings').addEventListener('click', closeSettings);
document.getElementById('settings-modal').addEventListener('click', (e) => {
  if (e.target.id === 'settings-modal') closeSettings();
});
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeSettings(); });
document.getElementById('set-save').addEventListener('click', saveSettings);
document.getElementById('set-adv-save').addEventListener('click', saveAdvisorSettings);

// Clear the activity feed (the in-memory problems + executions, service-side).
document.getElementById('clear-activity').addEventListener('click', async () => {
  try { await invoke('clear_problems'); await invoke('clear_executions'); } catch (e) { console.error(e); }
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

// Fetch the USD→GBP rate so costs display in pounds
invoke('gbp_per_usd').then(r => { if (r > 0) gbpRate = r; }).catch(() => {});
