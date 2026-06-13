const invoke = window.__TAURI__.core.invoke;

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

async function refresh() {
  let status;
  try { status = await invoke('get_status'); }
  catch (e) { console.error('get_status failed', e); return; }

  // Status dot + text
  const dot = document.getElementById('status-dot');
  const txt = document.getElementById('status-text');
  dot.style.background = STATUS_COLORS[status.status] ?? 'var(--gray)';
  txt.textContent = status.error
    ? `Error: ${status.error}`
    : status.status.replace(/([A-Z])/g, ' $1').trim();

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

// Initial load + poll every 2 seconds
refresh();
setInterval(refresh, 2000);
