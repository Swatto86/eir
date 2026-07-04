const invoke = window.__TAURI__.core.invoke;

// ── Native feel ───────────────────────────────────────────────────────────────
// Suppress the webview's browser context menu everywhere — the biggest "this is
// a webpage" tell in a desktop app. Inputs keep normal keyboard editing.
window.addEventListener('contextmenu', (e) => e.preventDefault());

// Hide on close (tray remains active)
window.__TAURI__.window.getCurrentWindow().onCloseRequested((e) => {
  e.preventDefault();
  window.__TAURI__.window.getCurrentWindow().hide();
});

// ── Theme (dark / light / system, persisted) ─────────────────────────────────

const THEMES = ['system', 'light', 'dark'];
const THEME_ICON = { system: '◐', light: '☀', dark: '☾' };
const osDark = window.matchMedia('(prefers-color-scheme: dark)');

function themeChoice() {
  const t = localStorage.getItem('eirTheme');
  return THEMES.includes(t) ? t : 'system';
}

function applyTheme() {
  const choice = themeChoice();
  const resolved = choice === 'system' ? (osDark.matches ? 'dark' : 'light') : choice;
  document.documentElement.dataset.theme = resolved;
  document.getElementById('theme-ico').textContent = THEME_ICON[choice];
  document.getElementById('theme-label').textContent =
    choice.charAt(0).toUpperCase() + choice.slice(1);
}

document.getElementById('theme-btn').addEventListener('click', () => {
  const next = THEMES[(THEMES.indexOf(themeChoice()) + 1) % THEMES.length];
  localStorage.setItem('eirTheme', next);
  applyTheme();
});
osDark.addEventListener('change', applyTheme);
applyTheme();

// ── Navigation ────────────────────────────────────────────────────────────────

const VIEW_TITLES = {
  dashboard: 'Dashboard',
  ask: 'Ask Eir',
  approvals: 'Approvals',
  activity: 'Activity',
  updates: 'App Updates',
  disk: 'Disk Space',
  startup: 'Startup Apps',
  learned: 'What Eir Has Learned',
  settings: 'Settings',
  about: 'About',
};

function showView(name) {
  document.querySelectorAll('.view').forEach((v) =>
    v.classList.toggle('active', v.id === `view-${name}`));
  document.querySelectorAll('.nav-btn').forEach((b) =>
    b.classList.toggle('active', b.dataset.view === name));
  document.getElementById('view-title').textContent = VIEW_TITLES[name] || name;
  if (name === 'settings') fillSettings();
}

document.getElementById('nav').addEventListener('click', (e) => {
  const btn = e.target.closest('.nav-btn');
  if (btn) showView(btn.dataset.view);
});
document.getElementById('dash-approvals-go').addEventListener('click', () => showView('approvals'));

// ── Formatting helpers ────────────────────────────────────────────────────────

function esc(s) {
  return String(s ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}
function escAttr(s) { return esc(s).replace(/"/g, '&quot;'); }
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

function fmtTokens(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K';
  return String(n);
}

let gbpRate = 0.79; // USD→GBP; refreshed from gbp_per_usd on load
function fmtGbp(usd) { return '£' + ((usd || 0) * gbpRate).toFixed(2); }

// One entry per service status: [dot colour, dashboard headline].
const STATUS_META = {
  Active:              ['var(--green)',  'Healthy — watching for problems'],
  Warning:             ['var(--yellow)', 'Problems found'],
  PendingApproval:     ['var(--orange)', 'Waiting for your approval'],
  Executing:           ['var(--blue)',   'Applying a fix'],
  Error:               ['var(--red)',    'Error'],
  ServiceDisconnected: ['var(--red)',    'Service disconnected'],
  Restarting:          ['var(--gray)',   'Service restarting…'],
  Connecting:          ['var(--gray)',   'Connecting to the service…'],
  Paused:              ['var(--gray)',   'Paused'],
  Initializing:        ['var(--gray)',   'Starting up…'],
};

function providerName(p) {
  return ({
    openrouter: 'OpenRouter',
    claude_cli: 'Claude CLI',
    anthropic: 'Claude (Anthropic)',
    kilo_cli: 'Kilo CLI',
  })[p] || p || '';
}

// Model (and effort) used for issue analysis — the main monitoring loop.
function analysisLabel(s) {
  if (!s) return '';
  let model = (s.model || '').trim();
  if (!model) {
    model = s.provider === 'openrouter' ? 'openrouter/free'
      : s.provider === 'claude_cli' ? 'default model'
      : s.provider === 'kilo_cli' ? 'default model'
      : '(no model set)';
  }
  let label = `${providerName(s.provider)} · ${model}`;
  const effort = (s.effort || '').trim();
  if (effort) label += ` · ${effort} effort`;
  return label;
}

// Which provider/model the app-update web check uses.
function updateCheckLabel(s) {
  if (!s) return '';
  const m = (s.update_check_model || '').trim();
  if (s.provider === 'anthropic') {
    return `Claude · ${m || 'claude-haiku-4-5'} + web`;
  }
  if (s.provider === 'claude_cli') {
    const lower = m.toLowerCase();
    const isClaude = ['haiku', 'sonnet', 'opus'].includes(lower) || lower.startsWith('claude');
    return `Claude CLI · ${isClaude ? m : 'haiku'} + web`;
  }
  if (s.provider === 'kilo_cli') {
    return `Kilo CLI · ${m || 'main model'} + web`;
  }
  const main = (s.model || '').trim() || (s.provider === 'openrouter' ? 'openrouter/free' : '');
  const web = s.provider === 'openrouter' ? ' + web' : '';
  return `${providerName(s.provider)} · ${m || main}${web}`;
}

// ── Refresh loop ──────────────────────────────────────────────────────────────

let lastStatus = null;

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

let refreshing = false;
async function refresh() {
  // Skip a tick if the previous get_status is still in flight (slow pipe), so
  // renders can't overlap and stomp each other.
  if (refreshing) return;
  refreshing = true;
  try {
    await refreshInner();
  } finally {
    refreshing = false;
  }
}

async function refreshInner() {
  let status;
  try { status = await invoke('get_status'); }
  catch (e) { console.error('get_status failed', e); return; }
  lastStatus = status;

  const [color, headline] = STATUS_META[status.status] ?? ['var(--gray)', status.status];
  document.getElementById('status-dot').style.background = color;
  document.getElementById('status-text').textContent =
    status.status.replace(/([A-Z])/g, ' $1').trim();

  // Dashboard hero
  document.getElementById('hero').style.setProperty('--hero-color', color);
  document.getElementById('hero-status').textContent = headline;
  const err = document.getElementById('hero-err');
  err.style.display = status.error ? 'block' : 'none';
  err.textContent = status.error || '';

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

  document.getElementById('pause-label').textContent = status.paused ? 'Resume' : 'Pause';

  document.getElementById('cpu').textContent    = pct(status.cpu);
  document.getElementById('memory').textContent = pct(status.memory);
  document.getElementById('disk').textContent   = pct(status.disk);
  setBar('cpu-bar',  status.cpu);
  setBar('mem-bar',  status.memory);
  setBar('disk-bar', status.disk);

  // Failed services
  const svcCard = document.getElementById('services-card');
  if (status.failed_services && status.failed_services.length > 0) {
    svcCard.style.display = 'block';
    document.getElementById('services-list').innerHTML = status.failed_services
      .map((s) => `<span class="svc-chip">${esc(s)}</span>`)
      .join('');
  } else {
    svcCard.style.display = 'none';
  }

  // Approvals (view + nav badge + dashboard cta)
  const pending = status.pending_approvals || [];
  renderApprovals(pending);
  const badge = document.getElementById('nav-approvals');
  badge.textContent = pending.length;
  badge.classList.toggle('on', pending.length > 0);
  document.getElementById('dash-approvals-card').style.display = pending.length ? 'block' : 'none';
  document.getElementById('dash-approvals-count').textContent = pending.length;

  renderAiNow(status);
  renderActivity(status);
  renderUsage(status.usage);
  renderUpdater(status.updater);
  renderLearned(status.learned_facts);
  renderDigest(status.digest);
  renderHistory(status);
  renderAsk(status.ask);
  renderDisk(status.disk_insights, status.paused);
  renderStartup(status.startup, status.paused);

  if (status.error && /settings|not applied/i.test(status.error)) {
    const ss = document.getElementById('set-status');
    if (ss) ss.textContent = status.error;
  }
}

// ── Weekly health digest ───────────────────────────────────────────────────────

function renderDigest(d) {
  const card = document.getElementById('digest-card');
  if (!card) return;
  if (!d || !d.text) { card.style.display = 'none'; return; }
  card.style.display = 'block';
  document.getElementById('digest-text').textContent = d.text;
  document.getElementById('digest-when').textContent =
    d.generated_at ? ago(d.generated_at) : '';
}

// ── Health timeline (24h sparklines) ────────────────────────────────────────────

// One sparkline SVG: a polyline of `key` over the points' time span, 0–100% on Y,
// with marker dots for problems/fixes. preserveAspectRatio="none" stretches the fixed
// viewBox to the card width. No chart library — hand-built to match the no-dep frontend.
function sparkline(points, key, color, markers) {
  const W = 240, H = 44, pad = 3;
  const t0 = points[0].at;
  const t1 = points[points.length - 1].at;
  const span = Math.max(1, t1 - t0);
  const x = (at) => pad + (W - 2 * pad) * (at - t0) / span;
  const y = (v) => pad + (H - 2 * pad) * (1 - Math.min(100, Math.max(0, v)) / 100);
  const d = points.map((p, i) => `${i ? 'L' : 'M'}${x(p.at).toFixed(1)},${y(p[key]).toFixed(1)}`).join(' ');
  const dots = markers
    .filter((m) => m.at >= t0 && m.at <= t1)
    .map((m) => `<circle cx="${x(m.at).toFixed(1)}" cy="${(H - 2).toFixed(1)}" r="2.2" fill="${m.color}"><title>${escAttr(m.label)}</title></circle>`)
    .join('');
  return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none">
    <line class="spark-axis" x1="0" y1="${(H - 1).toFixed(1)}" x2="${W}" y2="${(H - 1).toFixed(1)}"/>
    <path d="${d}" fill="none" stroke="${color}" stroke-width="1.5" vector-effect="non-scaling-stroke"/>
    ${dots}</svg>`;
}

function sparkCell(label, key, color, points, markers, latest) {
  return `<div class="spark">
    <div class="spark-head"><span class="spark-label">${label}</span><span class="spark-val" style="color:${color}">${pct(latest)}</span></div>
    ${sparkline(points, key, color, markers)}
  </div>`;
}

function renderHistory(status) {
  const card = document.getElementById('history-card');
  const pts = status.history || [];
  if (pts.length < 2) { card.style.display = 'none'; return; }
  card.style.display = 'block';
  const markers = [];
  for (const p of (status.recent_problems || [])) {
    if (p.at) markers.push({ at: p.at, color: p.blocked ? 'var(--red)' : 'var(--blue)', label: p.diagnosis });
  }
  for (const e of (status.recent_executions || [])) {
    if (e.at) markers.push({ at: e.at, color: e.success ? 'var(--green)' : 'var(--red)', label: e.action });
  }
  const last = pts[pts.length - 1];
  document.getElementById('spark-grid').innerHTML =
    sparkCell('CPU', 'cpu', 'var(--blue)', pts, markers, last.cpu) +
    sparkCell('Memory', 'memory', 'var(--blue)', pts, markers, last.memory) +
    sparkCell('Disk', 'disk', last.disk >= 90 ? 'var(--red)' : 'var(--accent)', pts, markers, last.disk);
}

// ── Ask Eir ──────────────────────────────────────────────────────────────────

let lastAskSig = null;
function renderAsk(ask) {
  const list = document.getElementById('ask-list');
  const sendBtn = document.getElementById('ask-send');
  const statusEl = document.getElementById('ask-status');
  const running = !!(ask && ask.running);
  sendBtn.disabled = running;
  sendBtn.textContent = running ? 'Thinking…' : 'Ask Eir';
  // Reconcile the status line with the service each poll; the transient "Sending…"
  // set by submitAsk is replaced here once the service reflects running/error.
  statusEl.textContent = running ? '' : ((ask && ask.error) || '');
  const entries = (ask && ask.entries) || [];
  const sig = JSON.stringify({ r: running, e: entries.map((x) => x.at) });
  if (sig === lastAskSig) return;
  lastAskSig = sig;
  let html = running ? '<div class="card"><div class="ask-spinner">Eir is thinking…</div></div>' : '';
  html += entries.map((e) => `
    <div class="card ask-entry">
      <div class="ask-q"><span class="qmark">Q</span><span>${esc(e.question)}</span></div>
      <div class="ask-a">${esc(e.answer)}</div>
      <div class="ask-when">${ago(e.at)}</div>
    </div>`).join('');
  list.innerHTML = html;
}

async function submitAsk() {
  const input = document.getElementById('ask-input');
  const q = input.value.trim();
  if (!q) return;
  const statusEl = document.getElementById('ask-status');
  statusEl.textContent = 'Sending…';
  try {
    await invoke('ask_eir', { question: q });
    input.value = '';
  } catch (e) {
    statusEl.textContent = 'Failed: ' + e;
  }
  refresh();
}

document.getElementById('ask-send').addEventListener('click', submitAsk);
document.getElementById('ask-input').addEventListener('keydown', (e) => {
  // Ctrl/Cmd+Enter submits (plain Enter makes newlines in the textarea).
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); submitAsk(); }
});

// ── Disk space ───────────────────────────────────────────────────────────────

function humanBytes(n) {
  n = n || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return (i === 0 ? n : v.toFixed(1)) + ' ' + u[i];
}

let lastDiskSig = null;
function renderDisk(di, paused) {
  const stateEl = document.getElementById('disk-state');
  const errEl = document.getElementById('disk-error');
  const list = document.getElementById('disk-list');
  const btn = document.getElementById('disk-scan');
  const running = !!(di && di.running);
  // Disable while scanning, and while paused (the service ignores a scan when paused,
  // so reflect that rather than dropping the click silently).
  btn.disabled = running || !!paused;
  btn.title = paused ? 'Guardian is paused — resume to scan' : '';
  btn.textContent = running ? 'Scanning…' : 'Scan now';
  stateEl.textContent = (di && di.scanned_at) ? '· scanned ' + ago(di.scanned_at) : (running ? '· scanning…' : '');
  errEl.style.display = (di && di.error) ? 'block' : 'none';
  errEl.textContent = (di && di.error) || '';
  const entries = (di && di.entries) || [];
  const sig = JSON.stringify({ r: running, e: entries, at: di && di.scanned_at });
  if (sig === lastDiskSig) return;
  lastDiskSig = sig;
  if (running && !entries.length) { list.innerHTML = '<div class="empty">Scanning your disk… this can take a minute.</div>'; return; }
  if (!entries.length) {
    list.innerHTML = (di && di.scanned_at)
      ? '<div class="empty">Nothing significant to clean up.</div>'
      : '<div class="empty">Click “Scan now” to find what\'s taking up space.</div>';
    return;
  }
  list.innerHTML = entries.map(diskRow).join('');
}

function diskRow(e) {
  const clean = e.cleanable
    ? `<button class="upd-mini disk-clean" data-id="${escAttr(e.id)}" title="Clean this up (via Eir's safety checks)">Clean</button>`
    : '<span class="di-pill tag-warn">report-only</span>';
  return `<div class="di-row">
    <div class="di-main">
      <div class="di-name" title="${escAttr(e.path)}">${esc(e.path)}</div>
      ${e.note ? `<div class="di-note">${esc(e.note)}</div>` : ''}
    </div>
    <span class="di-cat">${esc(e.category)}</span>
    <span class="di-size">${humanBytes(e.size_bytes)}</span>
    ${clean}
  </div>`;
}

document.getElementById('disk-scan').addEventListener('click', () => {
  invoke('scan_disk').catch((err) => console.error('scan_disk failed', err));
});
document.getElementById('disk-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.disk-clean');
  if (!btn) return;
  // Disable briefly to stop a double-submit, then re-enable — the command is
  // fire-and-forget (resolves once queued, not once applied), and the row isn't
  // repainted unless the entry set changes, so we must recover the button ourselves.
  // Re-clicks are safe: the service dedupes via in_flight/pending and rate-limits.
  btn.disabled = true;
  invoke('clean_disk_entry', { id: btn.dataset.id })
    .catch((err) => console.error('clean_disk_entry failed', err))
    .finally(() => setTimeout(() => { btn.disabled = false; }, 2500));
});

// ── Startup apps ─────────────────────────────────────────────────────────────

const STARTUP_VERDICT = {
  keep: '<span class="di-pill tag-ok">Keep</span>',
  optional: '<span class="di-pill tag-warn">Optional</span>',
  unnecessary: '<span class="di-pill tag-block">Unnecessary</span>',
};
const STARTUP_LOC = {
  hkcu_run: 'Registry (you)',
  hklm_run: 'Registry (all users)',
  startup_folder: 'Startup folder',
  common_startup_folder: 'Startup folder (all users)',
  scheduled_task: 'Scheduled task',
};

let lastStartupSig = null;
function renderStartup(sv, paused) {
  const stateEl = document.getElementById('startup-state');
  const errEl = document.getElementById('startup-error');
  const list = document.getElementById('startup-list');
  const btn = document.getElementById('startup-scan');
  const running = !!(sv && sv.running);
  btn.disabled = running || !!paused;
  btn.title = paused ? 'Guardian is paused — resume to scan' : '';
  btn.textContent = running ? 'Scanning…' : 'Scan now';
  stateEl.textContent = (sv && sv.scanned_at) ? '· scanned ' + ago(sv.scanned_at) : (running ? '· scanning…' : '');
  errEl.style.display = (sv && sv.error) ? 'block' : 'none';
  errEl.textContent = (sv && sv.error) || '';
  const entries = (sv && sv.entries) || [];
  const sig = JSON.stringify({ r: running, e: entries, at: sv && sv.scanned_at });
  if (sig === lastStartupSig) return;
  lastStartupSig = sig;
  if (running && !entries.length) { list.innerHTML = '<div class="empty">Scanning startup entries…</div>'; return; }
  if (!entries.length) {
    list.innerHTML = (sv && sv.scanned_at)
      ? '<div class="empty">No startup entries found.</div>'
      : '<div class="empty">Click “Scan now” to see what starts with Windows.</div>';
    return;
  }
  list.innerHTML = entries.map(startupRow).join('');
}

function startupRow(e) {
  const verdict = STARTUP_VERDICT[e.verdict] || '';
  const loc = STARTUP_LOC[e.location] || e.location;
  const toggle = e.enabled
    ? `<button class="upd-mini startup-toggle" data-id="${escAttr(e.id)}" data-enable="0" title="Stop this launching at sign-in">Disable</button>`
    : `<button class="upd-mini startup-toggle" data-id="${escAttr(e.id)}" data-enable="1" title="Let this launch at sign-in again">Enable</button>`;
  const state = e.enabled ? '' : '<span class="di-pill tag-block">Disabled</span>';
  return `<div class="di-row"${e.enabled ? '' : ' style="opacity:.55"'}>
    <div class="di-main">
      <div class="di-name" title="${escAttr(e.command)}">${esc(e.name)}</div>
      <div class="di-note">${esc(loc)}${e.note ? ' — ' + esc(e.note) : ''}</div>
    </div>
    ${verdict}${state}
    ${toggle}
  </div>`;
}

document.getElementById('startup-scan').addEventListener('click', () => {
  invoke('scan_startup').catch((err) => console.error('scan_startup failed', err));
});
document.getElementById('startup-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.startup-toggle');
  if (!btn) return;
  const enable = btn.dataset.enable === '1';
  // Same recovery as disk Clean: brief disable to block a double-submit, then re-enable.
  // The toggle is approval-gated and server-side deduped, so a re-click is harmless.
  btn.disabled = true;
  invoke('set_startup_entry', { id: btn.dataset.id, enable })
    .catch((err) => console.error('set_startup_entry failed', err))
    .finally(() => setTimeout(() => { btn.disabled = false; }, 2500));
});

// ── Approvals ────────────────────────────────────────────────────────────────

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

// Approval ids whose decide() call is still in flight — used to keep their buttons
// disabled across a re-render so a decision can't be double-submitted.
const decidingIds = new Set();
// Signature of the currently-rendered approval set. The 2s refresh only rebuilds the
// list when this changes, so it no longer wipes text selection or re-enables the
// buttons of an approval the user just acted on (before the service drops it).
let lastApprovalsSig = null;

function renderApprovals(list) {
  const el = document.getElementById('approvals');
  const sig = list.map((i) => `${i.id}:${i.created_at}`).join('|');
  if (sig === lastApprovalsSig) return;
  lastApprovalsSig = sig;
  el.innerHTML = list.length
    ? list.map(approvalCard).join('')
    : '<div class="empty">Nothing needs your approval right now.</div>';
  // Re-disable buttons for any decision still resolving.
  for (const id of decidingIds) {
    const card = el.querySelector(`.approval-card[data-approval-id="${id}"]`);
    if (card) card.querySelectorAll('button').forEach((b) => (b.disabled = true));
  }
}

async function decide(id, approved, card) {
  decidingIds.add(id);
  if (card) card.querySelectorAll('button').forEach((b) => (b.disabled = true));
  try {
    await invoke('decide_approval', { id, approved });
  } catch (e) {
    console.error('decide_approval failed', e);
    if (card) card.querySelectorAll('button').forEach((b) => (b.disabled = false));
  } finally {
    decidingIds.delete(id);
  }
}

document.getElementById('approvals').addEventListener('click', (e) => {
  const btn = e.target.closest('.btn-approve, .btn-reject');
  if (!btn) return;
  const id = parseInt(btn.dataset.id, 10);
  if (!Number.isFinite(id)) return;
  decide(id, btn.classList.contains('btn-approve'), btn.closest('.approval-card'));
});

// ── AI-now + activity feed ────────────────────────────────────────────────────

function renderAiNow(status) {
  document.getElementById('ai-now-text').textContent =
    status.last_analysis || 'Waiting for the first analysis cycle…';
  const bits = [];
  const a = status.advisor;
  if (a && a.escalated) {
    bits.push(`<span class="tag tag-auto">⤴ escalated${a.escalation_model ? ' → ' + esc(a.escalation_model) : ''}</span>`);
    if (a.reason) bits.push(`<span>${esc(a.reason)}</span>`);
  } else if (a && a.enabled) {
    bits.push('<span class="tag tag-ok">advisor on</span>');
  }
  if (a && a.spent_today_usd) bits.push(`<span>escalation spend today ~${fmtGbp(a.spent_today_usd)}</span>`);
  document.getElementById('ai-now-meta').innerHTML = bits.join('');
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
    // A registry reset that captured its prior value carries an undo_id → offer a
    // one-click revert.
    const undoId = (e.undo_id === 0 || e.undo_id) ? e.undo_id : null;
    items.push({ at: e.at || 0, icon, undoId, head: `${exTag(e)}<span class="act-text" title="${escAttr(e.action)}">${esc(e.action)}</span>`, why: esc(e.preview || '') });
  }
  items.sort((a, b) => (b.at || 0) - (a.at || 0));
  return items;
}

function renderActivity(status) {
  const el = document.getElementById('activity-list');
  const items = activityItems(status);
  if (!items.length) { el.innerHTML = '<div class="empty">No activity yet</div>'; return; }
  el.innerHTML = items.map((it) => {
    const undo = (it.undoId === 0 || it.undoId)
      ? `<button class="act-undo" data-undo="${escAttr(String(it.undoId))}" title="Restore the previous registry value">↩ Undo</button>`
      : '';
    return `
    <div class="act-item">
      <div class="act-icon">${it.icon}</div>
      <div class="act-main">
        <div class="act-head">${it.head}<span class="act-when">${ago(it.at)}</span>${undo}</div>
        ${it.why ? `<div class="act-why">${it.why}</div>` : ''}
      </div>
    </div>`;
  }).join('');
}

// One-click registry undo (delegated from the activity list).
document.getElementById('activity-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.act-undo');
  if (!btn) return;
  const id = parseInt(btn.dataset.undo, 10);
  if (!Number.isFinite(id)) return;
  btn.disabled = true;
  invoke('undo_registry', { id })
    .catch((err) => { btn.disabled = false; console.error('undo_registry failed', err); });
});

document.getElementById('clear-activity').addEventListener('click', async () => {
  try { await invoke('clear_problems'); await invoke('clear_executions'); } catch (e) { console.error(e); }
  refresh();
});

// ── AI usage ──────────────────────────────────────────────────────────────────

function renderUsage(u) {
  const card = document.getElementById('usage-card');
  if (!u) { card.style.display = 'none'; return; }
  card.style.display = 'block';
  const provider = (lastStatus && lastStatus.settings && lastStatus.settings.provider) || '';
  // Claude CLI runs on the subscription: no charge, so cost cells show a dash
  // (the CLI's figures are only the equivalent API cost).
  const free = provider === 'claude_cli';
  const costCell = (c) => (free ? '—' : fmtGbp(c));
  const note = provider === 'openrouter'
    ? 'OpenRouter-reported cost — £0.00 on free models.'
    : provider === 'claude_cli'
      ? 'No charge — uses your Claude subscription. Token counts shown for transparency.'
      : provider === 'anthropic'
        ? 'Estimated from Anthropic list pricing.'
        : 'Provider-reported cost where available.';
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

// ── App updates ───────────────────────────────────────────────────────────────

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
  // Ignore state comes from the service (a.ignored), so the toggle survives the 2s
  // poll instead of relying on a client-side style that the next render clobbers.
  const ign = !!a.ignored;
  const btn = ign
    ? `<button class="upd-mini upd-ignore" data-id="${escAttr(a.id)}" data-ignore="0" title="Resume checking this app">Unignore</button>`
    : `<button class="upd-mini upd-ignore" data-id="${escAttr(a.id)}" data-ignore="1" title="Don't check this app again">Ignore</button>`;
  return `<div class="upd-row${ign ? ' upd-ignored' : ''}" data-id="${escAttr(a.id)}"${ign ? ' style="opacity:.5"' : ''}>
    <span class="upd-name" title="${escAttr(a.name)}">${esc(a.name)}</span>
    <span class="upd-ver">${ver}</span>${meth}${badge}
    ${btn}
    ${detail}
  </div>`;
}

let lastAppsSig = null;
let lastNotesSig = null;
let lastHistSig = null;

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
  // The service ignores a manual run unless the updater is enabled (the master
  // switch also gates the pipe-triggered run), so reflect that in the button.
  nowBtn.disabled = !!u.running || !u.enabled;
  nowBtn.textContent = u.running ? 'Updating…' : '⬆ Update now';
  nowBtn.title = u.enabled ? '' : 'Enable auto-updates in Settings first';

  const bits = [];
  if (u.last_run) bits.push('last run ' + ago(u.last_run));
  if (u.enabled && u.next_run) bits.push('next in ' + until(u.next_run));
  if (u.last_cost_usd) bits.push('~' + fmtGbp(u.last_cost_usd));
  metaEl.style.display = bits.length ? 'block' : 'none';
  metaEl.textContent = bits.join(' · ');

  // Only rebuild the apps list when its content changes, so the 2s poll doesn't wipe
  // a text selection or the toggle you just clicked. The time-based meta above still
  // refreshes every poll. Phase/running are in the sig so the live stage still moves.
  const appsSig = JSON.stringify({ a: u.apps || [], r: u.running, p: u.phase, lr: u.last_run, en: u.enabled });
  if (appsSig !== lastAppsSig) {
    lastAppsSig = appsSig;
    if (u.apps && u.apps.length) {
      appsEl.innerHTML = u.apps.map(updaterAppRow).join('');
    } else if (u.running) {
      const phase = (u.phase && u.phase !== 'idle') ? u.phase : 'Checking for updates…';
      appsEl.innerHTML = `<div class="empty">${esc(phase)}</div>`;
    } else if (u.last_run) {
      appsEl.innerHTML = '<div class="empty">Everything up to date.</div>';
    } else {
      appsEl.innerHTML = '<div class="empty">Enable auto-updates in Settings, or click “Update now”.</div>';
    }
  }

  const notesSig = JSON.stringify(u.notes || []);
  if (notesSig !== lastNotesSig) {
    lastNotesSig = notesSig;
    notesEl.innerHTML = (u.notes && u.notes.length)
      ? u.notes.map((n) => `<div class="upd-note">• ${esc(n)}</div>`).join('') : '';
  }

  const histSig = JSON.stringify(u.recent || []);
  if (histSig !== lastHistSig) {
    lastHistSig = histSig;
    if (u.recent && u.recent.length) {
      histWrap.style.display = 'block';
      histEl.innerHTML = u.recent.slice(0, 15).map((r) =>
        `<div class="upd-note">${r.success ? '✓' : '✗'} ${esc(r.name)} ` +
        `<span style="opacity:.7">(${esc(methodLabel(r.method))})</span>` +
        `${r.detail ? ' — ' + esc(r.detail) : ''} <span class="row-age">${ago(r.at)}</span></div>`
      ).join('');
    } else {
      histWrap.style.display = 'none';
    }
  }
}

document.getElementById('upd-now').addEventListener('click', async () => {
  try { await invoke('run_updates_now'); } catch (e) { console.error('run_updates_now failed', e); }
});
document.getElementById('clear-updates').addEventListener('click', async () => {
  try { await invoke('clear_update_history'); } catch (e) { console.error('clear_update_history failed', e); }
  refresh();
});

// Per-app "Ignore" / "Unignore" — toggle checking this app (delegated from the list).
// The service echoes the ignored flag on the row, so the next poll re-renders the
// correct state; the immediate opacity nudge just avoids a 2s lag before that.
document.getElementById('updater-apps').addEventListener('click', (e) => {
  const ig = e.target.closest('.upd-ignore');
  if (!ig) return;
  const ignore = ig.dataset.ignore !== '0';
  invoke('set_app_ignore', { id: ig.dataset.id, ignore, note: '' })
    .then(() => { const row = ig.closest('.upd-row'); if (row) row.style.opacity = ignore ? '.5' : ''; })
    .catch((err) => console.error('set_app_ignore failed', err));
});

// ── Learned facts ─────────────────────────────────────────────────────────────

const LEARNED_BADGE = {
  user_pinned:   '<span class="upd-badge tag-ok">Pinned</span>',
  user_disabled: '<span class="upd-badge tag-block">Disabled</span>',
  expired:       '<span class="upd-badge tag-warn">Lapsed</span>',
};

function learnedRow(f) {
  const badge = LEARNED_BADGE[f.status] || '';
  const dim = (f.status === 'user_disabled' || f.status === 'expired') ? ' style="opacity:.55"' : '';
  const ai = f.source === 'ai_labelled' ? '<span class="upd-status">AI</span>' : '';
  return `<div class="upd-row"${dim} data-id="${f.id}">
    <span class="upd-name" title="${escAttr(f.detail)}">${esc(f.summary)}</span>${ai}${badge}
    <button class="upd-mini learned-act" data-id="${f.id}" data-op="pin"     title="Always keep this">Pin</button>
    <button class="upd-mini learned-act" data-id="${f.id}" data-op="disable" title="Ignore this learned fact">Disable</button>
    <button class="upd-mini learned-act" data-id="${f.id}" data-op="forget"  title="Delete (re-learns if it recurs)">Forget</button>
  </div>`;
}

let lastLearnedSig = null;

function renderLearned(facts) {
  const list = document.getElementById('learned-list');
  // Skip the innerHTML rebuild when nothing changed, so the 2s poll doesn't wipe a
  // text selection in the list (same guard as renderApprovals).
  const sig = JSON.stringify(facts || []);
  if (sig === lastLearnedSig) return;
  lastLearnedSig = sig;
  if (!facts || facts.length === 0) {
    list.innerHTML = '<div class="empty">Nothing learned yet — Eir records patterns (like apps that update themselves) as it works.</div>';
    return;
  }
  list.innerHTML = facts.map(learnedRow).join('');
}

document.getElementById('learned-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.learned-act');
  if (!btn) return;
  const id = parseInt(btn.dataset.id, 10);
  if (!Number.isFinite(id)) return;
  invoke('set_learned_fact', { id, op: btn.dataset.op })
    .then(refresh)
    .catch((err) => console.error('set_learned_fact failed', err));
});

// ── Settings ──────────────────────────────────────────────────────────────────

const PROVIDER_HINTS = {
  openrouter: 'One key, hundreds of models — free ones included. Blank model auto-routes to a free model. Key: openrouter.ai/keys',
  claude_cli: 'Uses your Claude subscription via the logged-in claude CLI — no API key. Auto-detects your profile and claude.exe. Blank model = the CLI default; aliases like haiku/sonnet/opus work.',
  anthropic: 'Claude direct from Anthropic. A model is required (e.g. claude-opus-4-8, claude-haiku-4-5). Key: console.anthropic.com',
  kilo_cli: 'Kilo CLI (your Kilo subscription) — no API key; borrows your logged-in Kilo session. Install with `npm install -g @kilocode/cli`, then run `kilo` once to sign in. Model needs the kilo/ prefix to route through your subscription/BYOK, e.g. kilo/minimax/minimax-m2.5.',
};

function updateProviderHint() {
  const p = document.getElementById('set-provider').value;
  document.getElementById('provider-hint').textContent = PROVIDER_HINTS[p] || '';
}
document.getElementById('set-provider').addEventListener('change', updateProviderHint);

async function fillAutostartSetting() {
  const box = document.getElementById('set-autostart');
  const st = document.getElementById('set-autostart-status');
  box.disabled = true;
  st.textContent = 'Checking…';
  try {
    box.checked = await invoke('get_autostart_enabled');
    st.textContent = '';
  } catch (e) {
    st.textContent = 'Unavailable: ' + e;
  } finally {
    box.disabled = false;
  }
}

async function saveAutostartSetting() {
  const box = document.getElementById('set-autostart');
  const st = document.getElementById('set-autostart-status');
  const enabled = box.checked;
  box.disabled = true;
  st.textContent = 'Saving…';
  try {
    box.checked = await invoke('set_autostart_enabled', { enabled });
    st.textContent = 'Saved — applies immediately.';
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  } finally {
    box.disabled = false;
  }
}

function fillSettings() {
  fillAutostartSetting();
  const s = lastStatus && lastStatus.settings;
  if (!s) return;
  document.getElementById('set-provider').value = s.provider || 'openrouter';
  updateProviderHint();
  document.getElementById('set-model').value = s.model || '';
  document.getElementById('set-effort').value = s.effort || '';
  document.getElementById('set-upd-model').value = s.update_check_model || '';
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
  document.getElementById('set-kilo-profile').placeholder =
    s.kilo_cli_user_profile_set ? '•••••• set — blank keeps it' : 'C:\\Users\\You  (blank = auto-detect)';
  document.getElementById('set-kilo-path').placeholder =
    s.kilo_cli_path_set ? '•••••• set — blank keeps it' : 'kilo  (blank = on PATH)';
  fillUpdaterSettings(lastStatus.updater && lastStatus.updater.settings);
  fillAdvisorSettings(lastStatus.advisor && lastStatus.advisor.settings);
}

async function saveSettings() {
  const splitList = (v) => v.split(/[,\n]/).map((x) => x.trim()).filter(Boolean);
  const orKey = document.getElementById('set-or-key').value.trim();
  const anKey = document.getElementById('set-an-key').value.trim();
  const kiloProfile = document.getElementById('set-kilo-profile').value.trim();
  const kiloPath = document.getElementById('set-kilo-path').value.trim();
  const settings = {
    provider: document.getElementById('set-provider').value,
    model: document.getElementById('set-model').value.trim(),
    effort: document.getElementById('set-effort').value,
    update_check_model: document.getElementById('set-upd-model').value.trim(),
    openrouter_api_key: orKey || null,
    anthropic_api_key: anKey || null,
    kilo_cli_user_profile: kiloProfile || null,
    kilo_cli_path: kiloPath || null,
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
      st.textContent = 'Claude needs an Anthropic API key — enter one above, then Save.';
      return;
    }
    if (!settings.model) {
      st.textContent = 'Claude needs a model — e.g. claude-opus-4-8 or claude-haiku-4-5';
      return;
    }
  }
  if (settings.provider === 'kilo_cli') {
    if (!settings.model) {
      st.textContent = 'Kilo CLI needs a model — e.g. kilo/minimax/minimax-m2.5 or anthropic/claude-sonnet-4.6';
      return;
    }
  }

  st.textContent = 'Saving… the service will restart (~15s).';
  try {
    await invoke('update_settings', { settings });
    st.textContent = 'Saved. Service restarting — it will reconnect shortly.';
    document.getElementById('set-or-key').value = '';
    document.getElementById('set-an-key').value = '';
  document.getElementById('set-kilo-profile').value = '';
  document.getElementById('set-kilo-path').value = '';
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  }
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

document.getElementById('set-save').addEventListener('click', saveSettings);
document.getElementById('set-adv-save').addEventListener('click', saveAdvisorSettings);
document.getElementById('set-upd-save').addEventListener('click', saveUpdaterSettings);
document.getElementById('set-autostart-save').addEventListener('click', saveAutostartSetting);

// ── Pause ─────────────────────────────────────────────────────────────────────

document.getElementById('pause-btn').addEventListener('click', async () => {
  await invoke('toggle_pause');
  refresh();
});

// ── About ─────────────────────────────────────────────────────────────────────

invoke('get_app_version')
  .then((v) => { document.getElementById('about-version').textContent = `Version ${v}`; })
  .catch(() => {});

document.getElementById('about-github').addEventListener('click', () => {
  invoke('open_url', { url: 'https://github.com/Swatto86/eir' }).catch(() => {});
});

document.getElementById('about-updates').addEventListener('click', async () => {
  const st = document.getElementById('about-status');
  st.textContent = 'Checking…';
  try {
    st.textContent = await invoke('check_updates_now');
  } catch (e) {
    st.textContent = 'Check failed: ' + e;
  }
});

// ── Boot ──────────────────────────────────────────────────────────────────────

refresh();
setInterval(refresh, 2000);

// Fetch the USD→GBP rate so costs display in pounds
invoke('gbp_per_usd').then((r) => { if (r > 0) gbpRate = r; }).catch(() => {});
