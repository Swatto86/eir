const invoke = window.__TAURI__.core.invoke;

// Escape hides the window (tray-app convention). Inside a field it just blurs,
// so a stray Escape while typing doesn't yank the window away.
window.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  const el = document.activeElement;
  if (el && /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName)) { el.blur(); return; }
  window.__TAURI__.window.getCurrentWindow().hide();
});

// ── Toasts (transient action feedback) ────────────────────────────────────────
// New services return applied/rejected command results; older services return an
// explicit queued-without-confirmation message. Toast the result instead of making
// the user infer it from the next 2s poll.
const toastWrap = document.getElementById('toast-wrap');
function toast(msg, kind = '') {
  const t = document.createElement('div');
  t.className = 'toast' + (kind ? ' ' + kind : '');
  t.textContent = msg;
  const kill = () => { t.classList.add('leaving'); setTimeout(() => t.remove(), 220); };
  const timer = setTimeout(kill, kind === 'err' ? 5000 : 2600);
  t.addEventListener('click', () => { clearTimeout(timer); kill(); });
  toastWrap.appendChild(t);
  // Cap the stack so a burst of clicks can't fill the screen.
  while (toastWrap.children.length > 4) toastWrap.firstChild.remove();
}

function commandMessage(result, fallback) {
  return (typeof result === 'string' && result.trim()) ? result : fallback;
}

const SERVICE_ACTION_SELECTOR = [
  '#ask-send', '#clear-ask',
  '#disk-scan', '.disk-clean',
  '#startup-scan', '.startup-toggle',
  '.btn-approve', '.btn-reject', '.btn-ignore-fix', '.btn-always-approve',
  '.act-undo', '#clear-activity', '#refresh-status',
  '#upd-now', '#clear-updates', '.upd-retry', '.upd-ignore', '.upd-note-save',
  '.learned-act', '.pref-clear', '#set-save', '#test-provider', '#set-adv-save', '#set-upd-save',
  '#pause-btn', '#game-btn',
].join(',');

function setServiceActionsDisconnected(disconnected) {
  document.querySelectorAll(SERVICE_ACTION_SELECTOR).forEach((button) => {
    if (disconnected) {
      button.dataset.serviceUnavailable = '1';
      button.setAttribute('aria-disabled', 'true');
    } else if (button.dataset.serviceUnavailable) {
      delete button.dataset.serviceUnavailable;
      button.removeAttribute('aria-disabled');
    }
  });
}

// `pointer-events` never blocks keyboard activation. Enforce aria-disabled at the
// event boundary too; Rust remains the trust-boundary backstop.
document.addEventListener('click', (e) => {
  if (!e.target.closest('[data-service-unavailable]')) return;
  e.preventDefault();
  e.stopImmediatePropagation();
}, true);
setServiceActionsDisconnected(true);

// Copy to clipboard with a plain-DOM fallback for webviews where navigator.clipboard
// is unavailable. Confirms on the button when given one, else via a toast.
async function copyText(text, btn) {
  let ok = false;
  try {
    await navigator.clipboard.writeText(text);
    ok = true;
  } catch {
    let ta;
    try {
      ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      ok = document.execCommand('copy');
    } catch { ok = false; }
    finally { if (ta) ta.remove(); }
  }
  if (btn) {
    clearTimeout(btn._copyTimer);
    const old = btn.dataset.copyLabel || btn.textContent;
    btn.dataset.copyLabel = old;
    btn.textContent = ok ? '✓ Copied' : 'Copy failed';
    btn._copyTimer = setTimeout(() => {
      btn.textContent = old;
      delete btn.dataset.copyLabel;
      delete btn._copyTimer;
    }, 1500);
  } else {
    toast(ok ? 'Copied to clipboard' : 'Copy failed', ok ? 'ok' : 'err');
  }
}

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
  learned: 'Learned & Preferences',
  settings: 'Settings',
  about: 'About',
};
let runtimePortable = null;

function showView(name) {
  document.querySelectorAll('.view').forEach((v) =>
    v.classList.toggle('active', v.id === `view-${name}`));
  document.querySelectorAll('.nav-btn').forEach((b) => {
    const active = b.dataset.view === name;
    b.classList.toggle('active', active);
    if (active) b.setAttribute('aria-current', 'page');
    else b.removeAttribute('aria-current');
  });
  document.getElementById('view-title').textContent = VIEW_TITLES[name] || name;
  if (name === 'settings') {
    if (runtimePortable === false && !dirtyCards.has('card-autostart')) {
      fillAutostartSetting();
    }
    // Hydrate only from real service snapshots. Dirty cards are preserved, while
    // dependent cards retry until their updater/advisor snapshots arrive.
    if (!settingsHydrated) fillSettings();
  }
}

document.getElementById('nav').addEventListener('click', (e) => {
  const btn = e.target.closest('.nav-btn');
  if (btn) showView(btn.dataset.view);
});
document.getElementById('dash-approvals-go').addEventListener('click', () => showView('approvals'));
document.getElementById('hero-fix').addEventListener('click', () => showView('settings'));
document.getElementById('hero-copy').addEventListener('click', (e) => {
  copyText((lastStatus && lastStatus.error) || '', e.currentTarget);
});

// ── Formatting helpers ────────────────────────────────────────────────────────

function esc(s) {
  return String(s ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}
function escAttr(s) { return esc(s).replace(/"/g, '&quot;'); }
function pct(v) { return `${Math.round(v)}%`; }

// Write only when the value actually changed, so a 2s poll that re-renders
// identical text doesn't wipe an in-progress selection on user-selectable nodes.
function setText(el, v) { if (el && el.textContent !== v) el.textContent = v; }

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
  Gaming:              ['#8b5cf6',       'Game Mode — staying out of the way while you play'],
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
    codex_cli: 'Codex CLI',
    anthropic: 'Claude (Anthropic)',
    kilo_cli: 'Kilo CLI',
    ollama: 'Ollama',
  })[p] || p || '';
}

// Model (and effort) used for issue analysis — the main monitoring loop.
function analysisLabel(s) {
  if (!s) return '';
  let model = (s.model || '').trim();
  if (!model) {
    model = s.provider === 'openrouter' ? 'openrouter/free'
      : s.provider === 'claude_cli' ? 'default model'
      : s.provider === 'codex_cli' ? 'default model'
      : s.provider === 'kilo_cli' ? 'default model'
      : s.provider === 'ollama' ? '(no model set)'
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
  if (s.provider === 'codex_cli') {
    const main = (s.model || '').trim();
    return `Codex CLI · ${m || main || 'default model'} + web`;
  }
  if (s.provider === 'kilo_cli') {
    return `Kilo CLI · ${m || 'main model'} + web`;
  }
  if (s.provider === 'ollama') {
    const web = s.ollama_key_set ? ' + web' : ' (no web search key)';
    return `Ollama · ${m || 'main model'}${web}`;
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

function renderSignalHealth(status) {
  const el = document.getElementById('signal-health');
  const errors = status.signal_errors || [];
  const ageSeconds = status.signals_at
    ? Math.max(0, Math.floor(Date.now() / 1000 - status.signals_at))
    : 0;
  const poll = (status.settings && status.settings.wmi_poll_interval_secs) || 300;
  const stale = !!status.signals_at && ageSeconds > Math.max(120, poll * 2);
  if (!errors.length && !stale) {
    el.style.display = 'none';
    el.textContent = '';
    return;
  }
  const labels = {
    cpu: 'CPU', memory: 'memory', disk: 'disk', services: 'services',
    network: 'network', network_interfaces: 'network interfaces',
    network_errors: 'network error counters', disk_health: 'disk health',
    windows_update: 'Windows Update', firewall: 'firewall', defender: 'Defender',
    not_collected: 'waiting for the first collection',
  };
  const bits = [];
  if (errors.length) bits.push('Unavailable: ' + errors.map((x) => labels[x] || x).join(', '));
  if (stale) bits.push('readings stale');
  if (status.signals_at) bits.push('collected ' + ago(status.signals_at));
  el.textContent = bits.join(' · ');
  el.style.display = 'block';
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
  catch (e) {
    // Say so on screen: the status line is role="status", so the failure is both
    // seen and announced. Silently returning left "Initializing…" up forever with
    // the actions disabled and no stated reason. No toast — polling retries every
    // 2s and would flood. A later successful poll restores everything below.
    console.error('get_status failed', e);
    document.body.classList.add('svc-down');
    document.getElementById('status-dot').style.background = 'var(--red)';
    setText(document.getElementById('status-text'), 'Service unavailable');
    setText(document.getElementById('hero-status'), 'Service unavailable');
    setServiceActionsDisconnected(true);
    return;
  }
  lastStatus = status;

  // While the service is down/restarting the last snapshot stays on screen; mark
  // the body so action buttons visibly disable and stale metrics dim (the Rust
  // command gate is the real backstop — clicks here would otherwise be dropped).
  const svcDown = ['ServiceDisconnected', 'Connecting', 'Restarting'].includes(status.status);
  document.body.classList.toggle('svc-down', svcDown);
  const testProvider = document.getElementById('test-provider');
  const canTestProvider = (status.capabilities || []).includes('provider_test');
  testProvider.disabled = testProvider.dataset.busy === '1' || !canTestProvider || svcDown;
  testProvider.title = canTestProvider && !svcDown
    ? 'Tests the saved provider from the service account'
    : (svcDown ? 'The Eir service is not connected' : 'Update the Eir service to enable provider testing');

  const [color, headline] = STATUS_META[status.status] ?? ['var(--gray)', status.status];
  document.getElementById('status-dot').style.background = color;
  setText(document.getElementById('status-text'),
    status.status.replace(/([A-Z])/g, ' $1').trim());

  // Dashboard hero
  document.getElementById('hero').style.setProperty('--hero-color', color);
  setText(document.getElementById('hero-status'), headline);
  const err = document.getElementById('hero-err');
  const errDisp = status.error ? 'block' : 'none';
  if (err.style.display !== errDisp) err.style.display = errDisp;
  setText(err, status.error || '');
  // A configuration-shaped error gets a one-click route to where it's fixed.
  // Matched against the service's actual error strings ("not configured … fix it
  // in Settings", "Settings not applied", "Save settings", "config.toml") plus
  // auth failures; deliberately NOT bare "model"/"key"/"provider", which appear
  // in transient errors ("model API error", rate limits) that Settings can't fix.
  // The whole action row shows only when there's an error; the fix link within it
  // shows only for config-shaped errors (copy-error is always available when shown).
  const cfgErr = !!status.error
    && /not configured|settings|config\.toml|api.?key|unauthorized|authentication|401/i.test(status.error);
  document.getElementById('hero-actions').style.display = status.error ? 'flex' : 'none';
  document.getElementById('hero-fix').style.display = cfgErr ? 'inline-block' : 'none';

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
  document.getElementById('pause-ico').textContent = status.paused ? '▶' : '⏸';
  document.getElementById('pause-btn').classList.toggle('paused', !!status.paused);
  document.getElementById('pause-btn').setAttribute('aria-pressed', String(!!status.paused));
  const gaming = !!status.gaming;
  document.getElementById('game-btn').classList.toggle('active', gaming);
  document.getElementById('game-btn').setAttribute('aria-pressed', String(gaming));
  // Explicit ON/Off both ways so the state is never ambiguous.
  document.getElementById('game-label').textContent = gaming ? 'Game Mode: ON' : 'Game Mode: Off';
  document.getElementById('game-btn').title = gaming
    ? 'Game Mode is ON — updates & housekeeping paused. Click to turn off.'
    : 'Game Mode is off. Click to pause Eir’s updates & housekeeping while you play.';

  const signalErrors = new Set(status.signal_errors || []);
  const waitingForSignals = signalErrors.has('not_collected');
  setText(document.getElementById('cpu'), waitingForSignals || signalErrors.has('cpu') ? '—' : pct(status.cpu));
  setText(document.getElementById('memory'), waitingForSignals || signalErrors.has('memory') ? '—' : pct(status.memory));
  setText(document.getElementById('disk'), waitingForSignals || signalErrors.has('disk') ? '—' : pct(status.disk));
  setBar('cpu-bar', waitingForSignals || signalErrors.has('cpu') ? 0 : status.cpu);
  setBar('mem-bar', waitingForSignals || signalErrors.has('memory') ? 0 : status.memory);
  setBar('disk-bar', waitingForSignals || signalErrors.has('disk') ? 0 : status.disk);
  renderSignalHealth(status);

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
  renderUpdater(status.updater, status.paused, status.protocol_version, status.capabilities || []);
  const settingsView = document.getElementById('view-settings');
  if (!status.settings) {
    settingsHydrated = false;
    setServiceSettingsLoading(true);
  } else if (settingsView.classList.contains('active') && !settingsHydrated) {
    fillSettings();
  }
  renderLearned(status.learned_facts, status.action_preferences);
  renderDigest(status.digest);
  renderHistory(status);
  renderAsk(status.ask);
  renderDisk(status.disk_insights, status.paused);
  renderStartup(status.startup, status.paused);

  // Re-tick every relative age in one pass. Ages are baked into signature-guarded
  // lists at build time, so without this an approval that has sat for hours keeps
  // saying "just now" until its content changes. ago(0) → '' keeps zero-ts blank.
  // Hovering shows the absolute local time (set once — the ts never changes for
  // a given element; a rebuilt list gets fresh elements).
  document.querySelectorAll('[data-ts]').forEach((el) => {
    const ts = +el.dataset.ts;
    setText(el, ago(ts));
    if (ts && !el.title) el.title = new Date(ts * 1000).toLocaleString();
  });

  if (status.error && /settings|not applied/i.test(status.error)) {
    const ss = document.getElementById('set-status');
    if (ss) ss.textContent = status.error;
  }
  setServiceActionsDisconnected(svcDown);
}

// ── Weekly health digest ───────────────────────────────────────────────────────

function renderDigest(d) {
  const card = document.getElementById('digest-card');
  if (!card) return;
  if (!d || !d.text) { card.style.display = 'none'; return; }
  card.style.display = 'block';
  setText(document.getElementById('digest-text'), d.text);
  setText(document.getElementById('digest-when'), d.generated_at ? ago(d.generated_at) : '');
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

let lastHistorySig = null;
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
  // Skip the rebuild when nothing changed, so the 2s poll doesn't recreate the SVGs
  // and dismiss a marker <title> tooltip mid-hover (same guard as the list renders).
  const sig = JSON.stringify({ pts, markers });
  if (sig === lastHistorySig) return;
  lastHistorySig = sig;
  const last = pts[pts.length - 1];
  document.getElementById('spark-grid').innerHTML =
    sparkCell('CPU', 'cpu', 'var(--blue)', pts, markers, last.cpu) +
    sparkCell('Memory', 'memory', 'var(--blue)', pts, markers, last.memory) +
    sparkCell('Disk', 'disk', last.disk >= 90 ? 'var(--red)' : 'var(--accent)', pts, markers, last.disk);
}

// ── Ask Eir ──────────────────────────────────────────────────────────────────

// The stale error to suppress after a submit, until the service reflects the new
// question (running) or a genuinely different error arrives. null = nothing to
// suppress. Set by submitAsk. `askBaselineAt` bounds the suppression so a rare
// same-text re-rejection can't leave "Sending…" stuck forever.
let askBaselineErr = null;
let askBaselineAt = 0;

let lastAskSig = null;
let askWasRunning = false;
let askLastDoneAt = 0; // ms; when the last answer finished (mirrors the service's spend guard)
function renderAsk(ask) {
  const list = document.getElementById('ask-list');
  const sendBtn = document.getElementById('ask-send');
  const statusEl = document.getElementById('ask-status');
  const running = !!(ask && ask.running);
  if (askWasRunning && !running) {
    askLastDoneAt = Date.now();
    // Announce completion: the answer lands in #ask-list (not a live region) and the
    // disabled send button's label change is not reliably re-announced, so a screen
    // reader heard "Question accepted" and then silence. Errors are announced by the
    // role="status" line below; announce success through the aria-live toast region,
    // which also reaches a user who has navigated to another view. Guarded to a
    // genuinely fresh answer so a service-restart blip cannot fake one.
    const newestAt = ((ask && ask.entries) || []).reduce((m, x) => Math.max(m, x.at || 0), 0);
    if (!(ask && ask.error) && newestAt && Date.now() / 1000 - newestAt < 30) {
      toast('Eir has answered — see Ask Eir.', 'ok');
    }
  }
  askWasRunning = running;
  sendBtn.disabled = running || askSending || askAttachmentBusy;
  sendBtn.textContent = running ? 'Thinking…' : 'Ask Eir';
  updateAskControls();
  // Reconcile the status line with the service each poll. After a submit, hold a
  // neutral "Sending…" instead of flashing the *previous* answer's error for the
  // up-to-2s gap before the service flips `running`; a new rejection (different
  // text) shows immediately.
  if (running) {
    askBaselineErr = null;
    statusEl.textContent = '';
  } else {
    const errText = (ask && ask.error) || '';
    if (askBaselineErr !== null && errText === askBaselineErr && Date.now() - askBaselineAt < 4000) {
      statusEl.textContent = 'Sending…';
    } else {
      askBaselineErr = null;
      statusEl.textContent = errText;
    }
  }
  const entries = (ask && ask.entries) || [];
  const sig = JSON.stringify({ r: running, e: entries.map((x) => x.at) });
  if (sig === lastAskSig) return;
  lastAskSig = sig;
  let html = running ? '<div class="card"><div class="ask-spinner">Eir is thinking…</div></div>' : '';
  html += entries.map((e) => `
    <div class="card ask-entry">
      <div class="ask-q"><span class="qmark">Q</span><span>${esc(e.question)}</span></div>
      ${(e.attachments && e.attachments.length) ? `<div class="ask-att-tag">📎 ${e.attachments.map(esc).join(', ')}</div>` : ''}
      <div class="ask-a">${esc(e.answer)}</div>
      <div class="ask-when"><span data-ts="${e.at}">${ago(e.at)}</span><button class="copy-btn ask-copy">Copy answer</button></div>
    </div>`).join('');
  list.innerHTML = html;
}

let askSending = false;
let askAttachmentBusy = false;
function updateAskControls() {
  const disabled = askSending || askAttachmentBusy || askWasRunning;
  document.getElementById('ask-send').disabled = disabled;
  document.getElementById('ask-attach-files').disabled = disabled;
  document.getElementById('ask-attach-folder').disabled = disabled;
  document.getElementById('clear-ask').disabled = askSending || askWasRunning;
  document.querySelectorAll('#ask-attach-chips .rm').forEach((button) => {
    button.disabled = disabled;
  });
}
async function submitAsk() {
  if (document.getElementById('ask-send').dataset.serviceUnavailable) return;
  const input = document.getElementById('ask-input');
  const q = input.value.trim();
  if (!q) return;
  // Synchronous in-flight guard: the Send button only disables via the next 2s poll's
  // ask.running, so without this a fast second Enter/click could queue a duplicate ask
  // before the service flips running. Re-enabled in finally.
  if (askSending || askAttachmentBusy || askWasRunning) return;
  const statusEl = document.getElementById('ask-status');
  // Mirror the service's 15s between-questions guard for an older service that cannot
  // return a rejection before the tray consumes pending attachments.
  const entries = (lastStatus && lastStatus.ask && lastStatus.ask.entries) || [];
  const newestAt = entries.reduce((m, e) => Math.max(m, e.at || 0), 0) * 1000;
  if (Date.now() - Math.max(askLastDoneAt, newestAt) < 15000) {
    statusEl.textContent = 'Please wait a few seconds between questions.';
    return;
  }
  askSending = true;
  const sendBtn = document.getElementById('ask-send');
  const sentChips = askChips;
  askChips = [];
  renderAskChips();
  sendBtn.disabled = true;
  statusEl.textContent = 'Sending…';
  try {
    const result = await invoke('ask_eir', { question: q });
    // Remember the error currently on screen so renderAsk suppresses only it (not
    // a fresh rejection) until the service picks this question up.
    askBaselineErr = (lastStatus && lastStatus.ask && lastStatus.ask.error) || '';
    askBaselineAt = Date.now();
    input.value = '';
    statusEl.textContent = commandMessage(result, 'Question accepted');
  } catch (e) {
    // The backend restores only the failed batch ahead of any attachments picked
    // while this request was in flight; mirror that order in the visible chips.
    askChips = sentChips.concat(askChips);
    renderAskChips();
    statusEl.textContent = 'Failed: ' + e;
  } finally {
    askSending = false;
    updateAskControls();
  }
  refresh();
}

document.getElementById('ask-send').addEventListener('click', submitAsk);
document.getElementById('ask-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.ask-copy');
  if (btn) copyText(btn.closest('.ask-entry').querySelector('.ask-a').textContent, btn);
});
document.getElementById('clear-ask').addEventListener('click', async () => {
  if (!window.confirm('Clear this chat? This cannot be undone.')) return;
  try {
    const result = await invoke('clear_ask');
    toast(commandMessage(result, 'Chat cleared'), 'ok');
  } catch (e) { console.error('clear_ask failed', e); toast('Could not clear chat: ' + e, 'err'); }
  refresh();
});
document.getElementById('ask-input').addEventListener('keydown', (e) => {
  // Enter sends (chat convention); Shift+Enter inserts a newline. isComposing
  // guards the Enter that confirms an IME composition.
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); submitAsk(); }
});

// ── Ask attachments (mirrors the service's pending list via command returns) ────
let askChips = [];
function renderAskChips() {
  document.getElementById('ask-attach-chips').innerHTML = askChips.map((a, i) => `
    <span class="ask-chip" title="${escAttr(a.name)}">
      <span>${a.kind === 'image' ? '🖼' : '📄'}</span>
      <span class="nm">${esc(a.name)}</span>
      <button type="button" class="rm" data-i="${i}" aria-label="Remove ${escAttr(a.name)}">✕</button>
    </span>`).join('');
  updateAskControls();
}
async function pickAttachments(kind) {
  if (askAttachmentBusy || askSending || askWasRunning) return;
  askAttachmentBusy = true;
  updateAskControls();
  try {
    const res = await invoke('add_ask_attachments', { kind }) || {};
    askChips = res.attachments || [];
    renderAskChips();
    // A picked file that cannot be attached (PDF/DOCX and other binaries, or over a
    // cap) used to vanish silently, so the question was asked without it.
    if (res.full) {
      toast('Attachment limit reached — remove an attachment first', 'err');
    } else if (res.skipped) {
      toast(`${res.skipped} file${res.skipped === 1 ? '' : 's'} could not be attached (binary, unreadable, or over the size limit)`, 'err');
    } else if (askChips.length >= 12) {
      toast('Attachment limit reached (12)', 'ok');
    }
  } catch (e) { console.error('add_ask_attachments failed', e); toast('Could not attach', 'err'); }
  finally { askAttachmentBusy = false; updateAskControls(); }
}
document.getElementById('ask-attach-files').addEventListener('click', () => pickAttachments('files'));
document.getElementById('ask-attach-folder').addEventListener('click', () => pickAttachments('folder'));
document.getElementById('ask-attach-chips').addEventListener('click', async (e) => {
  const rm = e.target.closest('.rm');
  if (!rm || askAttachmentBusy || askSending || askWasRunning) return;
  const removedIndex = parseInt(rm.dataset.i, 10);
  if (!Number.isFinite(removedIndex)) return;
  let removed = false;
  askAttachmentBusy = true;
  updateAskControls();
  try {
    askChips = await invoke('remove_ask_attachment', { index: removedIndex }) || [];
    renderAskChips();
    removed = true;
  }
  catch (err) { console.error('remove_ask_attachment failed', err); toast('Could not remove attachment: ' + err, 'err'); }
  finally {
    askAttachmentBusy = false;
    updateAskControls();
    if (removed) {
      const removeButtons = document.querySelectorAll('#ask-attach-chips .rm');
      const next = removeButtons[Math.min(removedIndex, removeButtons.length - 1)]
        || document.getElementById('ask-attach-files');
      next.focus();
    }
  }
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
  const entries = (di && di.entries) || [];
  const bits = [];
  if (di && di.scanned_at) bits.push('scanned ' + ago(di.scanned_at));
  else if (running) bits.push('scanning…');
  const cleanable = entries.filter((x) => x.cleanable).reduce((a, x) => a + (x.size_bytes || 0), 0);
  if (cleanable > 0) bits.push(humanBytes(cleanable) + ' cleanable');
  stateEl.textContent = bits.length ? '· ' + bits.join(' · ') : '';
  errEl.style.display = (di && di.error) ? 'block' : 'none';
  setText(errEl, (di && di.error) || '');
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

document.getElementById('disk-scan').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  btn.textContent = 'Starting…';
  try {
    const result = await invoke('scan_disk');
    toast(commandMessage(result, 'Disk scan started'), 'ok');
  } catch (err) {
    console.error('scan_disk failed', err);
    toast('Could not start disk scan: ' + err, 'err');
  }
  refresh();
});
document.getElementById('disk-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.disk-clean');
  if (!btn) return;
  // Disable briefly to stop a double-submit; the row may not repaint until the
  // approval/activity state changes.
  btn.disabled = true;
  invoke('clean_disk_entry', { id: btn.dataset.id })
    .then((result) => toast(commandMessage(result, 'Cleanup accepted — check Activity'), 'ok'))
    .catch((err) => { console.error('clean_disk_entry failed', err); toast('Could not clean up: ' + err, 'err'); })
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
  hklm_run32: 'Registry (32-bit, all users)',
  startup_folder: 'Startup folder',
  common_startup_folder: 'Startup folder (all users)',
  run_once: 'RunOnce (one-shot)',
  policies_run: 'Policy Run key',
  winlogon: 'Winlogon (system)',
  scheduled_task: 'Scheduled task (sign-in)',
  service: 'Auto-start service',
};

// "Hide Windows entries" filter (Microsoft-signed binaries), persisted. Default on —
// like Autoruns' Hide Microsoft Entries, the signal is in the third-party rows.
let startupHideMs = localStorage.getItem('eirStartupHideMs') !== '0';
const hideMsBox = document.getElementById('startup-hide-ms');
hideMsBox.checked = startupHideMs;
hideMsBox.addEventListener('change', () => {
  startupHideMs = hideMsBox.checked;
  localStorage.setItem('eirStartupHideMs', startupHideMs ? '1' : '0');
  lastStartupSig = null; // force the list to rebuild with the new filter
  refresh();
});

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
  const all = (sv && sv.entries) || [];
  const hidden = startupHideMs ? all.filter((x) => x.microsoft).length : 0;
  const entries = hidden ? all.filter((x) => !x.microsoft) : all;
  const bits = [];
  if (sv && sv.scanned_at) bits.push('scanned ' + ago(sv.scanned_at));
  else if (running) bits.push('scanning…');
  if (all.length) {
    const off = all.filter((x) => !x.enabled).length;
    bits.push(all.length + (all.length === 1 ? ' entry' : ' entries') + (off ? `, ${off} disabled` : ''));
    if (hidden) bits.push(`${hidden} Windows hidden`);
  }
  stateEl.textContent = bits.length ? '· ' + bits.join(' · ') : '';
  errEl.style.display = (sv && sv.error) ? 'block' : 'none';
  setText(errEl, (sv && sv.error) || '');
  const sig = JSON.stringify({ r: running, e: entries, at: sv && sv.scanned_at, h: startupHideMs });
  if (sig === lastStartupSig) return;
  lastStartupSig = sig;
  if (running && !all.length) { list.innerHTML = '<div class="empty">Scanning startup entries…</div>'; return; }
  if (!all.length) {
    list.innerHTML = (sv && sv.scanned_at)
      ? '<div class="empty">No startup entries found.</div>'
      : '<div class="empty">Click “Scan now” to see what starts with Windows.</div>';
    return;
  }
  if (!entries.length) {
    list.innerHTML = '<div class="empty">All entries are Windows components — untick “Hide Windows” to see them.</div>';
    return;
  }
  list.innerHTML = entries.map(startupRow).join('');
}

function startupRow(e) {
  const verdict = STARTUP_VERDICT[e.verdict] || '';
  const loc = STARTUP_LOC[e.location] || e.location;
  const signer = e.signer
    ? `<span class="di-cat" title="Publisher, from the file's digital signature">${esc(e.signer)}</span>`
    : '<span class="di-cat" title="No digital signature found on the launched file">unsigned</span>';
  const control = e.report_only
    ? '<span class="di-pill tag-warn" title="Listed for awareness — Eir has no safe switch for this location">report-only</span>'
    : (e.enabled
      ? `<button class="upd-mini startup-toggle" data-id="${escAttr(e.id)}" data-enable="0" title="Stop this launching at sign-in">Disable</button>`
      : `<button class="upd-mini startup-toggle" data-id="${escAttr(e.id)}" data-enable="1" title="Let this launch at sign-in again">Enable</button>`);
  const state = e.enabled ? '' : '<span class="di-pill tag-block">Disabled</span>';
  return `<div class="di-row"${e.enabled ? '' : ' style="opacity:.55"'}>
    <div class="di-main">
      <div class="di-name" title="${escAttr(e.command)}">${esc(e.name)}</div>
      <div class="di-note">${esc(loc)}${e.note ? ' — ' + esc(e.note) : ''}</div>
    </div>
    ${signer}${verdict}${state}
    ${control}
  </div>`;
}

document.getElementById('startup-scan').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  btn.textContent = 'Starting…';
  try {
    const result = await invoke('scan_startup');
    toast(commandMessage(result, 'Startup scan started'), 'ok');
  } catch (err) {
    console.error('scan_startup failed', err);
    toast('Could not start startup scan: ' + err, 'err');
  }
  refresh();
});
document.getElementById('startup-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.startup-toggle');
  if (!btn) return;
  const enable = btn.dataset.enable === '1';
  // Same recovery as disk Clean: brief disable to block a double-submit, then re-enable.
  // The toggle is approval-gated and server-side deduped, so a re-click is harmless.
  btn.disabled = true;
  invoke('set_startup_entry', { id: btn.dataset.id, enable })
    .then((result) => toast(commandMessage(result, `${enable ? 'Enable' : 'Disable'} accepted`), 'ok'))
    .catch((err) => { console.error('set_startup_entry failed', err); toast('Could not change startup entry: ' + err, 'err'); })
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
      <h2>⚠ Approval needed<span class="appr-age" data-ts="${info.created_at}">${ago(info.created_at)}</span></h2>
      <div class="appr-what">
        <div class="appr-what-label">What this will do</div>
        <div class="appr-what-text">${esc(info.action_summary || info.action)}</div>
        <div class="appr-flags">${flag}</div>
      </div>
      <div class="appr-target">
        <span class="appr-target-label">Target</span>
        <code class="appr-target-val">${esc(info.target || '—')}</code>
        <button class="copy-btn appr-copy" title="Copy target and details">Copy</button>
        ${details}
      </div>
      <div class="approval-grid">${grid}</div>
      <div class="approval-actions">
        <button class="btn-approve" data-id="${info.id}"${info.reversible ? '' : ' data-irreversible="1"'}>Approve &amp; run</button>
        <button class="btn-always-approve" data-id="${info.id}"${info.reversible ? '' : ' data-irreversible="1"'} title="Approve now and auto-approve this fix when it comes up again">Always approve</button>
        <button class="btn-reject"  data-id="${info.id}">Reject</button>
        <button class="btn-ignore-fix" data-id="${info.id}" title="Dismiss and do not ask about this fix again">Ignore</button>
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
  // The rebuild destroys whatever inside the list had focus; move it somewhere
  // stable rather than letting it fall to <body>.
  const hadFocus = el.contains(document.activeElement);
  el.innerHTML = list.length
    ? list.map(approvalCard).join('')
    : '<div class="empty">Nothing needs your approval right now.</div>';
  if (hadFocus) document.querySelector('.nav-btn[data-view="approvals"]').focus();
  // Re-disable buttons for any decision still resolving.
  for (const id of decidingIds) {
    const card = el.querySelector(`.approval-card[data-approval-id="${id}"]`);
    if (card) card.querySelectorAll('button').forEach((b) => (b.disabled = true));
  }
}

async function decide(id, approved, card) {
  decidingIds.add(id);
  if (card) {
    // Disabling the focused button blurs it to <body>, and the 2s poll then rebuilds
    // the whole list — a keyboard user would restart from the top of the document
    // after every decision. Park focus on the stable Approvals nav button first; a
    // stray second Enter there merely re-shows the view.
    if (card.contains(document.activeElement)) {
      document.querySelector('.nav-btn[data-view="approvals"]').focus();
    }
    card.querySelectorAll('button').forEach((b) => (b.disabled = true));
  }
  try {
    const result = await invoke('decide_approval', { id, approved });
    toast(commandMessage(result, approved ? 'Approval applied' : 'Rejection applied'), 'ok');
  } catch (e) {
    console.error('decide_approval failed', e);
    toast('Decision failed: ' + e, 'err');
    if (card) {
      card.querySelectorAll('button').forEach((b) => (b.disabled = false));
      // Disarm any armed irreversible-confirm button; otherwise it stays stuck showing
      // "cannot be undone" with nothing actually pending confirmation after a failed send.
      card.querySelectorAll('.btn-approve.confirm, .btn-always-approve.confirm').forEach((b) => {
        clearTimeout(b._confirmTimer);
        b.classList.remove('confirm');
        b.textContent = b.classList.contains('btn-always-approve') ? 'Always approve' : 'Approve & run';
      });
    }
  } finally {
    decidingIds.delete(id);
  }
}

async function setApprovalPreference(id, preference, card) {
  decidingIds.add(id);
  if (card) {
    if (card.contains(document.activeElement)) {
      document.querySelector('.nav-btn[data-view="approvals"]').focus();
    }
    card.querySelectorAll('button').forEach((b) => (b.disabled = true));
  }
  try {
    const result = await invoke('set_action_preference', { id, preference });
    const fallback = preference === 'ignore'
      ? 'Fix ignored; it will not ask again'
      : 'Always approved; action queued';
    toast(commandMessage(result, fallback), 'ok');
  } catch (e) {
    console.error('set_action_preference failed', e);
    toast('Could not save preference: ' + e, 'err');
    if (card) {
      card.querySelectorAll('button').forEach((b) => (b.disabled = false));
      card.querySelectorAll('.btn-always-approve.confirm').forEach((b) => {
        clearTimeout(b._confirmTimer);
        b.classList.remove('confirm');
        b.textContent = 'Always approve';
      });
    }
  } finally {
    decidingIds.delete(id);
  }
}

document.getElementById('approvals').addEventListener('click', (e) => {
  const copyBtn = e.target.closest('.appr-copy');
  if (copyBtn) {
    const card = copyBtn.closest('.approval-card');
    const target = card.querySelector('.appr-target-val');
    const details = card.querySelector('.appr-details');
    const text = [target && target.textContent, details && details.textContent]
      .filter(Boolean).join('\n');
    copyText(text, copyBtn);
    return;
  }
  const prefBtn = e.target.closest('.btn-ignore-fix, .btn-always-approve');
  if (prefBtn) {
    const id = parseInt(prefBtn.dataset.id, 10);
    if (!Number.isFinite(id)) return;
    const preference = prefBtn.classList.contains('btn-ignore-fix') ? 'ignore' : 'always_approve';
    if (preference === 'always_approve' && prefBtn.dataset.irreversible && !prefBtn.classList.contains('confirm')) {
      prefBtn.classList.add('confirm');
      prefBtn.textContent = 'Click again — always approve (cannot be undone)';
      toast('This action cannot be undone — activate Always approve again to confirm.', 'err');
      prefBtn._confirmTimer = setTimeout(() => {
        prefBtn.classList.remove('confirm');
        prefBtn.textContent = 'Always approve';
      }, 6000);
      return;
    }
    clearTimeout(prefBtn._confirmTimer);
    setApprovalPreference(id, preference, prefBtn.closest('.approval-card'));
    return;
  }
  const btn = e.target.closest('.btn-approve, .btn-reject');
  if (!btn) return;
  const id = parseInt(btn.dataset.id, 10);
  if (!Number.isFinite(id)) return;
  const approve = btn.classList.contains('btn-approve');
  // Approving an IRREVERSIBLE action takes two clicks: the first arms the button
  // for 6s, the second confirms. Reversible approvals and Reject stay one-click.
  if (approve && btn.dataset.irreversible && !btn.classList.contains('confirm')) {
    btn.classList.add('confirm');
    btn.textContent = 'Click again to confirm — cannot be undone';
    // A label swap on the already-focused button is not reliably re-announced, so
    // the warning reached sighted users only. #toast-wrap is aria-live="polite";
    // the 'err' variant lasts 5s, covering the 6s arm window.
    toast('This action cannot be undone — activate Approve again to confirm.', 'err');
    btn._confirmTimer = setTimeout(() => {
      btn.classList.remove('confirm');
      btn.textContent = 'Approve & run';
    }, 6000);
    return;
  }
  clearTimeout(btn._confirmTimer);
  decide(id, approve, btn.closest('.approval-card'));
});

// ── AI-now + activity feed ────────────────────────────────────────────────────

function renderAiNow(status) {
  setText(document.getElementById('ai-now-text'),
    status.last_analysis || 'Waiting for the first analysis cycle…');
  const bits = [];
  // When the last analysis ran — the "is it actually alive?" trust signal.
  if (status.last_analysis_at) bits.push(`<span>analysed ${ago(status.last_analysis_at)}</span>`);
  const a = status.advisor;
  if (a && a.escalated) {
    bits.push(`<span class="tag tag-auto">⤴ escalated${a.escalation_model ? ' → ' + esc(a.escalation_model) : ''}</span>`);
    if (a.reason) bits.push(`<span>${esc(a.reason)}</span>`);
  } else if (a && a.enabled) {
    bits.push('<span class="tag tag-ok">advisor on</span>');
  }
  if (a && a.spent_today_usd) bits.push(`<span>escalation spend today ~${fmtGbp(a.spent_today_usd)}</span>`);
  const meta = document.getElementById('ai-now-meta');
  const metaHtml = bits.join('');
  if (meta.innerHTML !== metaHtml) meta.innerHTML = metaHtml;
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
// `type` drives the filter chips; `fail` marks a blocked diagnosis or failed fix.
function activityItems(status) {
  const items = [];
  for (const p of (status.recent_problems || [])) {
    const icon = p.blocked ? '🚫' : (p.auto_executed ? '🔧' : '🔎');
    const why = [p.action, p.reason].filter(Boolean).map(esc).join(' — ');
    items.push({ at: p.at || 0, icon, type: 'diag', fail: !!p.blocked, head: `${problemTag(p)}<span class="act-text" title="${escAttr(p.diagnosis)}">${esc(p.diagnosis)}</span>`, why });
  }
  for (const e of (status.recent_executions || [])) {
    const icon = e.success ? '✅' : '❌';
    // A registry reset that captured its prior value carries an undo_id → offer a
    // one-click revert.
    const undoId = (e.undo_id === 0 || e.undo_id) ? e.undo_id : null;
    items.push({ at: e.at || 0, icon, undoId, type: 'fix', fail: !e.success, head: `${exTag(e)}<span class="act-text" title="${escAttr(e.action)}">${esc(e.action)}</span>`, why: esc(e.preview || '') });
  }
  items.sort((a, b) => (b.at || 0) - (a.at || 0));
  return items;
}

// Activity view filter: all | fix | diag | fail (failures across both kinds).
let activityFilter = 'all';
function matchesActivityFilter(it) {
  if (activityFilter === 'all') return true;
  if (activityFilter === 'fail') return it.fail;
  return it.type === activityFilter;
}

// Undo ids whose undo_registry call is still in flight — kept disabled across a
// re-render so the click can't be double-submitted (mirror of decidingIds).
const undoingIds = new Set();
// Signature of the currently-rendered activity list. The 2s poll only rebuilds
// when this changes, so it no longer wipes a text selection, kills a tooltip, or
// re-enables an Undo the user just clicked. Ages tick via the data-ts sweep.
let lastActivitySig = null;

function renderActivity(status) {
  const el = document.getElementById('activity-list');
  const all = activityItems(status);
  // Signature includes the filter so switching chips forces a rebuild; undo bookkeeping
  // uses the FULL list so an item hidden by the filter can't be forgotten prematurely.
  const items = all.filter(matchesActivityFilter);
  const sig = JSON.stringify({ f: activityFilter, items });
  if (sig === lastActivitySig) return;
  lastActivitySig = sig;
  // Forget in-flight undo ids the service has since cleared (their row no longer
  // carries the undo_id), so the set can't grow without bound.
  undoingIds.forEach((id) => { if (!all.some((it) => it.undoId === id)) undoingIds.delete(id); });
  if (!items.length) {
    el.innerHTML = all.length
      ? '<div class="empty">Nothing matches this filter.</div>'
      : '<div class="empty">No activity yet</div>';
    return;
  }
  el.innerHTML = items.map((it) => {
    const undo = (it.undoId === 0 || it.undoId)
      ? `<button class="act-undo" data-undo="${escAttr(String(it.undoId))}" title="Restore the previous registry value">↩ Undo</button>`
      : '';
    return `
    <div class="act-item">
      <div class="act-icon">${it.icon}</div>
      <div class="act-main">
        <div class="act-head">${it.head}<span class="act-when" data-ts="${it.at}">${ago(it.at)}</span>${undo}</div>
        ${it.why ? `<div class="act-why">${it.why}</div>` : ''}
      </div>
    </div>`;
  }).join('');
  // Re-disable any undo still resolving across this rebuild.
  for (const id of undoingIds) {
    const btn = el.querySelector(`.act-undo[data-undo="${id}"]`);
    if (btn) btn.disabled = true;
  }
}

// One-click registry undo (delegated from the activity list).
document.getElementById('activity-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.act-undo');
  if (!btn) return;
  const id = parseInt(btn.dataset.undo, 10);
  if (!Number.isFinite(id)) return;
  undoingIds.add(id);
  btn.disabled = true;
  invoke('undo_registry', { id })
    .then((result) => toast(commandMessage(result, 'Undo applied'), 'ok'))
    .catch((err) => { undoingIds.delete(id); btn.disabled = false; console.error('undo_registry failed', err); toast('Undo failed: ' + err, 'err'); });
});

document.getElementById('clear-activity').addEventListener('click', async () => {
  if (!window.confirm('Clear all activity history? This cannot be undone.')) return;
  try {
    await invoke('clear_problems');
    const result = await invoke('clear_executions');
    toast(commandMessage(result, 'Activity cleared'), 'ok');
  }
  catch (e) { console.error(e); toast('Could not clear activity: ' + e, 'err'); }
  refresh();
});

// Force a live-status refresh (fast services rescan on the service side).
document.getElementById('refresh-status').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  try {
    const result = await invoke('refresh_status');
    toast(commandMessage(result, 'Service status refreshed'), 'ok');
  }
  catch (err) { console.error('refresh_status failed', err); toast('Could not refresh: ' + err, 'err'); }
  finally { btn.disabled = false; }
});

// Activity filter chips — client-side, no service round-trip.
document.getElementById('activity-filter').addEventListener('click', (e) => {
  const chip = e.target.closest('.chip');
  if (!chip || chip.dataset.filter === activityFilter) return;
  activityFilter = chip.dataset.filter;
  document.querySelectorAll('#activity-filter .chip').forEach((c) => {
    const active = c === chip;
    c.classList.toggle('active', active);
    c.setAttribute('aria-pressed', String(active));
  });
  if (lastStatus) renderActivity(lastStatus);
});

// ── AI usage ──────────────────────────────────────────────────────────────────

let lastUsageSig = null;
function renderUsage(u) {
  const card = document.getElementById('usage-card');
  if (!u) { card.style.display = 'none'; return; }
  card.style.display = 'block';
  const provider = (lastStatus && lastStatus.settings && lastStatus.settings.provider) || '';
  // Skip the innerHTML rebuild when nothing changed, so the 2s poll doesn't churn
  // the card (and can't wipe a selection on the figures).
  // Include gbpRate: it starts at the 0.79 fallback and is replaced a few seconds after
  // boot by the live rate. Without it in the signature, the £ cells stay computed off the
  // stale rate until the usage numbers themselves next change.
  const usageSig = JSON.stringify({ u, provider, gbpRate });
  if (usageSig === lastUsageSig) return;
  lastUsageSig = usageSig;
  // Claude CLI runs on the subscription: no charge, so cost cells show a dash
  // (the CLI's figures are only the equivalent API cost).
  const free = provider === 'claude_cli' || provider === 'codex_cli' || provider === 'ollama';
  const costCell = (c) => (free ? '—' : fmtGbp(c));
  const note = provider === 'openrouter'
    ? 'OpenRouter-reported cost — £0.00 on free models.'
    : provider === 'claude_cli'
      ? 'No charge — uses your Claude subscription. Token counts shown for transparency.'
      : provider === 'codex_cli'
        ? 'No charge — uses your ChatGPT subscription. Token counts shown for transparency.'
      : provider === 'ollama'
        ? 'No charge — runs on your local Ollama server.'
      : provider === 'anthropic'
        ? 'Estimated from Anthropic list pricing.'
        : provider === 'kilo_cli'
          ? 'Kilo-reported cost — usually covered by your Kilo plan; real for BYOK models.'
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
  current:   '<span class="upd-badge tag-ok">Current</span>',
  verified:  '<span class="upd-badge tag-ok">Verified</span>',
  installed: '<span class="upd-badge tag-warn">Installed</span>',
  failed:    '<span class="upd-badge tag-block">Failed</span>',
  skipped:   '<span class="upd-badge tag-warn">Skipped</span>',
};

function methodLabel(m) {
  return ({ winget: 'winget', choco: 'Chocolatey', scoop: 'Scoop', msstore: 'Store', native: 'AI installer' })[m] || m || '';
}

function updaterNoteEditor(id, note) {
  return `<div class="upd-note-editor" data-id="${escAttr(id)}" hidden>
    <textarea class="upd-note-input" maxlength="2000" aria-label="AI guidance for ${escAttr(id)}" placeholder="Example: This is Acme’s Foo, not the unrelated namesake. Official downloads: https://…">${esc(note || '')}</textarea>
    <span class="upd-note-actions">
      <button class="upd-mini upd-note-save">Save</button>
      <button class="upd-mini upd-note-cancel">Cancel</button>
    </span>
  </div>`;
}

function updaterAppRow(a, retrySupported, running) {
  const badge = UPD_BADGE[a.state] || '';
  if (a.id === 'update-check' && !a.from && !a.to && !a.method) {
    return `<div class="upd-row" data-id="update-check">
      <span class="upd-name">${esc(a.name)}</span>${badge}
      <span class="upd-result">${esc(a.detail || '')}</span>
    </div>`;
  }
  const ver = `${esc(a.from || '?')}${a.to ? ' → ' + esc(a.to) : ''}`;
  const meth = a.method ? `<span class="upd-status">via ${esc(methodLabel(a.method))}</span>` : '';
  const detailText = [a.detail, a.signature].filter(Boolean).join(' · ');
  const detail = detailText ? `<span class="upd-result">${esc(detailText)}</span>` : '';
  // Ignore state comes from the service (a.ignored), so the toggle survives the 2s
  // poll instead of relying on a client-side style that the next render clobbers.
  const ign = !!a.ignored;
  if (ign) return ''; // Hidden from Updates Available; reversible from Settings → Ignored apps.
  const showGuidance = a.state === 'failed' || a.state === 'skipped';
  const note = showGuidance && a.note ? `<span class="upd-result">AI guidance: ${esc(a.note)}</span>` : '';
  const btn = `<button class="upd-mini upd-ignore" data-id="${escAttr(a.id)}" data-ignore="1" title="Hide from Updates Available and stop checking">Ignore</button>`;
  const retry = a.state === 'failed' && retrySupported
    ? `<button class="upd-mini upd-retry" data-id="${escAttr(a.id)}"${running ? ' disabled' : ''} title="Re-check only this app using the latest saved guidance">Retry</button>`
    : '';
  const guidanceButton = showGuidance
    ? `<button class="upd-mini upd-guide">${a.note ? 'Edit guidance' : 'Guide AI'}</button>`
    : '';
  const guidanceEditor = showGuidance ? updaterNoteEditor(a.id, a.note) : '';
  return `<div class="upd-row" data-id="${escAttr(a.id)}">
    <span class="upd-name" title="${escAttr(a.name)}">${esc(a.name)}</span>
    <span class="upd-ver">${ver}</span>${meth}${badge}
    ${guidanceButton}
    ${retry}
    ${btn}
    ${detail}${note}
    ${guidanceEditor}
  </div>`;
}

let lastAppsSig = null;
let lastNotesSig = null;
let lastHistSig = null;

function renderUpdater(u, paused, protocolVersion, capabilities) {
  const stateEl = document.getElementById('updater-state');
  const metaEl = document.getElementById('updater-meta');
  const appsEl = document.getElementById('updater-apps');
  const notesEl = document.getElementById('updater-notes');
  const histWrap = document.getElementById('updater-history-wrap');
  const histEl = document.getElementById('updater-history');
  const nowBtn = document.getElementById('upd-now');
  const clearBtn = document.getElementById('clear-updates');
  if (!u) {
    stateEl.textContent = '';
    metaEl.textContent = '';
    metaEl.style.display = 'none';
    appsEl.innerHTML = '';
    notesEl.innerHTML = '';
    histEl.innerHTML = '';
    histWrap.style.display = 'none';
    lastAppsSig = null;
    lastNotesSig = null;
    lastHistSig = null;
    nowBtn.disabled = true;
    clearBtn.disabled = true;
    nowBtn.textContent = '⬆ Update now';
    nowBtn.title = 'The running service does not report updater status';
    return;
  }

  const phase = (u.phase || 'idle').trim() || 'idle';
  stateEl.textContent = `· ${phase} · ${u.enabled ? 'auto' : 'schedule off'}`;
  // Manual runs remain useful with scheduling off; pause and an active run are
  // the only blockers.
  nowBtn.disabled = !!u.running || !!paused;
  clearBtn.disabled = !!u.running;
  nowBtn.textContent = u.running ? 'Updating…' : '⬆ Update now';
  nowBtn.title = paused ? 'Resume monitoring before starting updates' : '';

  const bits = [];
  if (u.last_run) bits.push('last run ' + ago(u.last_run));
  if (u.last_clean_run) bits.push('last clean run ' + ago(u.last_clean_run));
  if (u.enabled && u.next_run) bits.push('next in ' + until(u.next_run));
  if (u.last_cost_usd) bits.push('~' + fmtGbp(u.last_cost_usd));
  metaEl.style.display = bits.length ? 'block' : 'none';
  metaEl.textContent = bits.join(' · ');

  // Only rebuild the apps list when its content changes, so the 2s poll doesn't wipe
  // a text selection or the toggle you just clicked. The time-based meta above still
  // refreshes every poll. Phase/running are in the sig so the live stage still moves.
  const retrySupported = capabilities.includes('targeted_update_retry');
  const appsSig = JSON.stringify({
    a: u.apps || [], r: u.running, p: u.phase, lr: u.last_run,
    lc: u.last_clean_run, en: u.enabled, retry: retrySupported,
  });
  const appsDirty = appsEl.querySelector('.upd-note-editor:not([hidden]) .upd-note-input[data-dirty="1"]');
  if (appsSig !== lastAppsSig && !appsDirty) {
    lastAppsSig = appsSig;
    const visibleApps = (u.apps || []).filter((app) => !app.ignored);
    if (visibleApps.length) {
      appsEl.innerHTML = visibleApps.map((app) => updaterAppRow(app, retrySupported, u.running)).join('');
    } else if (u.running) {
      const phase = (u.phase && u.phase !== 'idle') ? u.phase : 'Checking for updates…';
      appsEl.innerHTML = `<div class="empty">${esc(phase)}</div>`;
    } else if (u.last_run && u.last_clean_run === u.last_run) {
      appsEl.innerHTML = '<div class="empty">No updates found in this check.</div>';
    } else if (u.last_run && protocolVersion >= 2) {
      appsEl.innerHTML = '<div class="empty">The check finished with warnings — review the notes below.</div>';
    } else if (u.last_run) {
      appsEl.innerHTML = '<div class="empty">Last check completed.</div>';
    } else {
      appsEl.innerHTML = '<div class="empty">Click “Update now” for a one-off run, or enable the schedule in Settings.</div>';
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
      histEl.innerHTML = u.recent.slice(0, 15).map((r) => {
        const version = [r.from_version, r.to_version].filter(Boolean).join(' → ');
        const evidence = [
          version,
          r.category,
          r.exit_code == null ? '' : `exit ${r.exit_code}`,
        ].filter(Boolean).join(' · ');
        const clean = r.success || r.category === 'already_current';
        return `<div class="upd-note">${clean ? '✓' : '✗'} ${esc(r.name)} ` +
          `<span style="opacity:.7">(${esc(methodLabel(r.method))})</span>` +
          `${evidence ? ' · ' + esc(evidence) : ''}` +
          `${r.detail ? ' — ' + esc(r.detail) : ''} <span class="row-age" data-ts="${r.at}">${ago(r.at)}</span></div>`;
      }).join('');
    } else {
      histWrap.style.display = 'none';
    }
  }
}

document.getElementById('upd-now').addEventListener('click', async (e) => {
  // Same synchronous in-progress state as the scan buttons: without it the button
  // stayed live for up to a poll (2s), inviting a second run. renderUpdater
  // reconciles the real label/disabled state on the refresh below.
  const btn = e.currentTarget;
  btn.disabled = true;
  btn.textContent = 'Starting…';
  try {
    const result = await invoke('run_updates_now');
    toast(commandMessage(result, 'Update run started'), 'ok');
  }
  catch (e) { console.error('run_updates_now failed', e); toast('Could not start updates: ' + e, 'err'); }
  refresh();
});
document.getElementById('clear-updates').addEventListener('click', async () => {
  if (!window.confirm('Clear all update history? This cannot be undone.')) return;
  try {
    const result = await invoke('clear_update_history');
    toast(commandMessage(result, 'Update history cleared'), 'ok');
  }
  catch (e) { console.error('clear_update_history failed', e); toast('Could not clear history: ' + e, 'err'); }
  refresh();
});

// Per-app "Ignore" / "Unignore" — toggle checking this app (delegated from the list).
// The service echoes the ignored flag on the row, so the next poll re-renders the
// correct state; the immediate opacity nudge just avoids a 2s lag before that.
document.getElementById('updater-apps').addEventListener('click', (e) => {
  const retry = e.target.closest('.upd-retry');
  if (retry) {
    retry.disabled = true;
    invoke('retry_app_update', { id: retry.dataset.id })
      .then((result) => toast(commandMessage(result, 'Update retry started'), 'ok'))
      .catch((err) => {
        console.error('retry_app_update failed', err);
        toast('Could not retry update: ' + err, 'err');
        retry.disabled = false;
      });
    return;
  }
  const ig = e.target.closest('.upd-ignore');
  if (!ig) return;
  const ignore = ig.dataset.ignore !== '0';
  ig.disabled = true;
  invoke('set_app_ignore', { id: ig.dataset.id, ignore, note: '' })
    .then((result) => {
      toast(commandMessage(result, ignore ? 'App ignored' : 'App unignored'), 'ok');
      refresh();
    })
    .catch((err) => { console.error('set_app_ignore failed', err); toast('Could not update ignore: ' + err, 'err'); })
    .finally(() => { ig.disabled = false; });
});

// Return focus to the row's guidance toggle when its editor closes — only when the
// focus is actually inside the editor being hidden, so a mouse user who clicked
// elsewhere isn't yanked back.
function focusGuidanceToggle(editor) {
  if (!editor.contains(document.activeElement)) return;
  const toggle = editor.closest('.upd-row')?.querySelector('.upd-guide');
  if (toggle) toggle.focus();
}

// Per-app AI guidance is shown and editable only while that product has an update row.
document.getElementById('view-updates').addEventListener('input', (e) => {
  if (e.target.classList.contains('upd-note-input')) e.target.dataset.dirty = '1';
});
document.getElementById('view-updates').addEventListener('click', (e) => {
  const edit = e.target.closest('.upd-guide');
  if (edit) {
    const editor = edit.closest('.upd-row').querySelector('.upd-note-editor');
    editor.hidden = false;
    editor.querySelector('.upd-note-input').focus();
    return;
  }
  const cancel = e.target.closest('.upd-note-cancel');
  if (cancel) {
    const editor = cancel.closest('.upd-note-editor');
    editor.hidden = true;
    // Hiding the editor blurs whatever inside it had focus; hand it back to the
    // button that opened it, as a disclosure should.
    focusGuidanceToggle(editor);
    lastAppsSig = null;
    refresh();
    return;
  }
  const save = e.target.closest('.upd-note-save');
  if (!save) return;
  const editor = save.closest('.upd-note-editor');
  const id = editor.dataset.id;
  const note = editor.querySelector('.upd-note-input').value.trim();
  const btn = save;
  btn.disabled = true;
  invoke('set_app_note', { id, note })
    .then((result) => {
      if (editor) {
        editor.hidden = true;
        focusGuidanceToggle(editor);
      }
      toast(commandMessage(result, note ? 'AI guidance saved' : 'AI guidance deleted'), 'ok');
    })
    .catch((err) => { console.error('set_app_note failed', err); toast('Could not update AI guidance: ' + err, 'err'); })
    .finally(() => { btn.disabled = false; });
});

// ── Learned facts + action preferences ───────────────────────────────────────

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
    <span class="upd-name">${esc(f.summary)}</span>${ai}${badge}
    <button class="upd-mini learned-act" data-id="${f.id}" data-op="pin"     title="Always keep this">Pin</button>
    <button class="upd-mini learned-act" data-id="${f.id}" data-op="disable" title="Ignore this learned fact">Disable</button>
    <button class="upd-mini learned-act" data-id="${f.id}" data-op="forget"  title="Delete (re-learns if it recurs)">Forget</button>
    ${f.detail ? `<details class="upd-result"><summary>Evidence</summary><div>${esc(f.detail)}</div></details>` : ''}
  </div>`;
}

function preferenceRow(p) {
  const kind = p.preference === 'always_approve'
    ? '<span class="upd-badge tag-ok">Always approve</span>'
    : '<span class="upd-badge tag-block">Ignored</span>';
  const clearLabel = p.preference === 'always_approve' ? 'Stop always approving' : 'Stop ignoring';
  const target = p.target ? `<span class="upd-result">Target: ${esc(p.target)}</span>` : '';
  return `<div class="upd-row" data-key="${escAttr(p.action_key)}">
    <span class="upd-name">${esc(p.summary || p.action_key)}</span>${kind}
    <button class="upd-mini pref-clear" data-key="${escAttr(p.action_key)}" title="${escAttr(clearLabel)}">${clearLabel}</button>
    ${target}
  </div>`;
}

let lastLearnedSig = null;

function renderLearned(facts, preferences) {
  const list = document.getElementById('learned-list');
  const prefList = document.getElementById('pref-list');
  // Skip the innerHTML rebuild when nothing changed, so the 2s poll doesn't wipe a
  // text selection in the list (same guard as renderApprovals).
  const sig = JSON.stringify({ f: facts || [], p: preferences || [] });
  if (sig === lastLearnedSig) return;
  lastLearnedSig = sig;
  if (!preferences || preferences.length === 0) {
    prefList.innerHTML = '<div class="empty">No Ignore or Always Approve preferences yet — set them from an Approvals card.</div>';
  } else {
    prefList.innerHTML = preferences.map(preferenceRow).join('');
  }
  if (!facts || facts.length === 0) {
    list.innerHTML = '<div class="empty">Nothing learned yet. After enough repeated patterns (self-updating apps, fixes that never help, or actions you keep rejecting), Eir adapts conservatively — skipping noisy updates or proposing a fix less readily. For a hard stop or auto-run, use Ignore / Always Approve on an Approvals card.</div>';
  } else {
    list.innerHTML = facts.map(learnedRow).join('');
  }
}

document.getElementById('pref-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.pref-clear');
  if (!btn) return;
  const actionKey = btn.dataset.key;
  if (!actionKey) return;
  btn.disabled = true;
  invoke('clear_action_preference', { action_key: actionKey })
    .then((result) => {
      toast(commandMessage(result, 'Preference cleared'), 'ok');
      refresh();
    })
    .catch((err) => {
      console.error('clear_action_preference failed', err);
      toast('Could not clear preference: ' + err, 'err');
      btn.disabled = false;
    });
});

document.getElementById('learned-list').addEventListener('click', (e) => {
  const btn = e.target.closest('.learned-act');
  if (!btn) return;
  const id = parseInt(btn.dataset.id, 10);
  if (!Number.isFinite(id)) return;
  // "Forget" is a hard row delete on the service side, and it sits right next to the
  // reversible Disable — confirm it like the Clear buttons do. Pin/Disable stay
  // one-click.
  const forget = btn.dataset.op === 'forget';
  if (forget
      && !window.confirm('Forget this learned fact? It is deleted permanently and only re-learned if the pattern happens again.')) {
    return;
  }
  // Disable the row's buttons to prevent a double-submit before the next repaint,
  // mirroring the approve / undo / disk-clean controls.
  const row = btn.parentElement;
  row.querySelectorAll('.learned-act').forEach((b) => { b.disabled = true; });
  invoke('set_learned_fact', { id, op: btn.dataset.op })
    .then((result) => {
      toast(commandMessage(result, forget ? 'Learned fact forgotten' : 'Learned fact updated'), 'ok');
      refresh();
    })
    .catch((err) => {
      console.error('set_learned_fact failed', err);
      toast('Could not update learned fact: ' + err, 'err');
      row.querySelectorAll('.learned-act').forEach((b) => { b.disabled = false; });
    });
});

// ── Settings ──────────────────────────────────────────────────────────────────

// Per-card dirty tracking: the Settings view is four independently-saved cards, so
// a save in one card must not clear unsaved edits in another. Any input marks its
// enclosing card dirty; a successful save clears only that card; reconnect hydration
// refreshes clean cards while retaining edits in dirty ones.
const dirtyCards = new Set();
let settingsHydrated = false;
const SERVICE_SETTINGS_CARD_IDS = ['card-provider', 'card-advisor', 'card-updater'];
function serviceSettingsCardAvailable(id) {
  if (!lastStatus || !lastStatus.settings
      || ['ServiceDisconnected', 'Connecting', 'Restarting'].includes(lastStatus.status)) {
    return false;
  }
  if (id === 'card-advisor') return !!(lastStatus.advisor && lastStatus.advisor.settings);
  if (id === 'card-updater') return !!(lastStatus.updater && lastStatus.updater.settings);
  return true;
}
function setServiceSettingsCardLoading(id, loading) {
  document.getElementById(id)
    .querySelectorAll('input, select, textarea, button')
    .forEach((control) => {
      if (loading || !['test-provider', 'm-scoop'].includes(control.id)) {
        control.disabled = loading;
      }
    });
}
function setServiceSettingsLoading(loading) {
  SERVICE_SETTINGS_CARD_IDS.forEach((id) => {
    setServiceSettingsCardLoading(id, loading || !serviceSettingsCardAvailable(id));
  });
  console.assert(loading || document.getElementById('m-scoop').disabled,
    'Scoop must remain unavailable after settings load');
}
setServiceSettingsLoading(true);
document.getElementById('set-autostart').disabled = true;
document.getElementById('set-autostart-save').disabled = true;
document.getElementById('view-settings').addEventListener('input', (e) => {
  const card = e.target.closest('.card');
  if (card && SERVICE_SETTINGS_CARD_IDS.includes(card.id)
      && !serviceSettingsCardAvailable(card.id)) return;
  if (card && card.id) dirtyCards.add(card.id);
});

// Read an integer input clamped to [min, max] (blank/garbage → def) so a typed
// out-of-range value saves as what the service will actually honour.
function numVal(id, def, min, max) {
  const input = document.getElementById(id);
  const v = parseInt(input.value, 10);
  const bounded = Math.min(max, Math.max(min, Number.isFinite(v) ? v : def));
  input.value = bounded;
  return bounded;
}

const PROVIDER_HINTS = {
  openrouter: 'One key, hundreds of models — free ones included. Blank model uses openrouter/free. Key: openrouter.ai/keys',
  claude_cli: 'Uses your Claude subscription via the logged-in claude CLI — no API key. Auto-detects your profile and claude.exe. Blank model = the CLI default; aliases like haiku/sonnet/opus work.',
  codex_cli: 'Uses your ChatGPT subscription via the logged-in Codex CLI — no API key. Install Codex and run `codex login` once; Eir auto-detects your profile and codex.exe.',
  anthropic: 'Claude direct from Anthropic. A model is required (e.g. claude-opus-4-8, claude-haiku-4-5). Key: console.anthropic.com',
  kilo_cli: 'Uses your Kilo subscription via the logged-in Kilo CLI — no API key. Install with `npm install -g @kilocode/cli`, then run `kilo` once to sign in. Subscription models use the kilo/ prefix.',
  ollama: 'Uses a local Ollama server for chat — no key needed for that. For app-update web search, add a free key from ollama.com/settings/keys. Only pulled models appear in the list.',
};

const PROVIDER_MODEL_PLACEHOLDERS = {
  openrouter: 'Choose a model; blank uses openrouter/free',
  claude_cli: 'blank = CLI default, or haiku / sonnet / opus',
  codex_cli: 'blank = CLI default, or choose a Codex model',
  anthropic: 'required, e.g. claude-opus-4-8 or claude-haiku-4-5',
  kilo_cli: 'Choose a kilo/ subscription model',
  ollama: 'required, e.g. llama3.2 or qwen2.5:7b',
};

const PROVIDER_UPDATE_PLACEHOLDERS = {
  openrouter: 'blank = main model, with web search',
  claude_cli: 'blank = Haiku, with web search',
  codex_cli: 'blank = main model, with web search',
  anthropic: 'blank = Claude Haiku, with web search',
  kilo_cli: 'blank = main model, with web search',
  ollama: 'blank = main model, with web search when key set',
};

const MODEL_INPUT_IDS = ['set-model', 'set-upd-model', 'set-adv-model'];
const modelValuesByProvider = new Map();
let activeModelProvider = '';
let savedProvider = '';
let modelListRequest = 0;

function currentModelValues() {
  return MODEL_INPUT_IDS.map((id) => document.getElementById(id).value);
}

function rememberModelValues(provider) {
  if (provider) modelValuesByProvider.set(provider, currentModelValues());
}

function restoreModelValues(provider) {
  const values = modelValuesByProvider.get(provider) || ['', '', ''];
  MODEL_INPUT_IDS.forEach((id, index) => {
    document.getElementById(id).value = values[index] || '';
  });
}

async function loadProviderModels(provider) {
  const request = ++modelListRequest;
  const list = document.getElementById('provider-models');
  const hint = document.getElementById('model-hint');
  list.replaceChildren();
  hint.textContent = 'Loading models…';
  MODEL_INPUT_IDS.forEach((id) => document.getElementById(id).setAttribute('aria-busy', 'true'));
  try {
    const args = { provider };
    if (provider === 'ollama') {
      args.ollamaBaseUrl = document.getElementById('set-ollama-url').value.trim() || null;
    }
    const models = await invoke('list_provider_models', args);
    if (request !== modelListRequest || provider !== document.getElementById('set-provider').value) return;
    const selected = currentModelValues().filter(Boolean);
    const values = [...new Set([...selected, ...(models || [])])];
    const fragment = document.createDocumentFragment();
    values.forEach((value) => {
      const option = document.createElement('option');
      option.value = value;
      fragment.appendChild(option);
    });
    list.replaceChildren(fragment);
    const countLabel = provider === 'ollama'
      ? `${values.length} pulled model${values.length === 1 ? '' : 's'}`
      : `${values.length} model${values.length === 1 ? '' : 's'}`;
    hint.textContent = `${countLabel} — open the list or type to filter.`;
  } catch (e) {
    if (request === modelListRequest) {
      const msg = typeof e === 'string' ? e : String(e);
      hint.textContent = provider === 'ollama'
        ? msg
        : 'Model list unavailable — the current model is still preserved.';
      if (provider !== 'ollama') console.error('list_provider_models failed', e);
    }
  } finally {
    if (request === modelListRequest) {
      MODEL_INPUT_IDS.forEach((id) => document.getElementById(id).removeAttribute('aria-busy'));
    }
  }
}

function updateProviderHint() {
  const provider = document.getElementById('set-provider').value;
  document.getElementById('provider-hint').textContent = PROVIDER_HINTS[provider] || '';
  document.getElementById('set-model').placeholder = PROVIDER_MODEL_PLACEHOLDERS[provider] || '';
  document.getElementById('set-upd-model').placeholder =
    PROVIDER_UPDATE_PLACEHOLDERS[provider] || '';
  document.querySelectorAll('[data-provider-only]').forEach((field) => {
    field.hidden = field.dataset.providerOnly !== provider;
  });
  loadProviderModels(provider);
}

function switchProvider() {
  const provider = document.getElementById('set-provider').value;
  rememberModelValues(activeModelProvider);
  activeModelProvider = provider;
  restoreModelValues(provider);
  updateProviderHint();
}
document.getElementById('set-provider').addEventListener('change', switchProvider);
document.getElementById('set-ollama-url').addEventListener('change', () => {
  if (document.getElementById('set-provider').value === 'ollama') {
    loadProviderModels('ollama');
  }
});

let autostartRequest = 0;
async function fillAutostartSetting() {
  const box = document.getElementById('set-autostart');
  const save = document.getElementById('set-autostart-save');
  const st = document.getElementById('set-autostart-status');
  const request = ++autostartRequest;
  box.disabled = true;
  save.disabled = true;
  st.textContent = 'Checking…';
  try {
    const enabled = await invoke('get_autostart_enabled');
    if (request !== autostartRequest) return;
    box.checked = enabled;
    st.textContent = '';
  } catch (e) {
    if (request === autostartRequest) st.textContent = 'Unavailable: ' + e;
  } finally {
    if (request === autostartRequest) {
      box.disabled = false;
      save.disabled = false;
    }
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
    dirtyCards.delete('card-autostart');
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  } finally {
    box.disabled = false;
  }
}

function fillSettings() {
  const s = lastStatus && lastStatus.settings;
  if (!s) return;
  document.getElementById('set-save').disabled = false;
  // A service restart invalidates the old snapshot, but must not erase edits the
  // user was typing when the pipe dropped. Rehydrate each clean card independently.
  if (!dirtyCards.has('card-provider')) {
    const provider = s.provider || 'openrouter';
    document.getElementById('set-provider').value = provider;
    activeModelProvider = provider;
    savedProvider = provider;
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
    document.getElementById('set-game-auto').checked = s.game_mode_auto !== false;
    document.getElementById('set-game-power').checked = !!s.game_mode_power_boost;
    document.getElementById('set-or-key').placeholder =
      s.openrouter_key_set ? '•••••• set — blank keeps it' : 'not set';
    document.getElementById('set-an-key').placeholder =
      s.anthropic_key_set ? '•••••• set — blank keeps it' : 'not set';
    document.getElementById('set-kilo-profile').placeholder =
      s.kilo_cli_user_profile_set ? '•••••• set — blank keeps it' : 'C:\\Users\\You  (blank = auto-detect)';
    document.getElementById('set-kilo-path').placeholder =
      s.kilo_cli_path_set ? '•••••• set — blank keeps it' : 'kilo  (blank = on PATH)';
    document.getElementById('set-ollama-url').value = s.ollama_base_url || 'http://127.0.0.1:11434/v1';
    document.getElementById('set-ollama-key').placeholder =
      s.ollama_key_set ? '•••••• set — blank keeps it' : 'ollama.com/settings/keys (for web search)';
    rememberModelValues(provider);
  }
  if (!dirtyCards.has('card-updater')) {
    fillUpdaterSettings(lastStatus.updater && lastStatus.updater.settings);
  }
  if (!dirtyCards.has('card-advisor')) {
    fillAdvisorSettings(lastStatus.advisor && lastStatus.advisor.settings);
  }
  // A nested snapshot can arrive a poll later than the main service settings.
  // Keep retrying clean-card hydration until both dependent cards have data.
  settingsHydrated = !!(lastStatus.updater && lastStatus.updater.settings
    && lastStatus.advisor && lastStatus.advisor.settings);
  setServiceSettingsLoading(false);
}

async function saveSettings() {
  const splitList = (v) => v.split(/[,\n]/).map((x) => x.trim()).filter(Boolean);
  const orKey = document.getElementById('set-or-key').value.trim();
  const anKey = document.getElementById('set-an-key').value.trim();
  const kiloProfile = document.getElementById('set-kilo-profile').value.trim();
  const kiloPath = document.getElementById('set-kilo-path').value.trim();
  const ollamaUrl = document.getElementById('set-ollama-url').value.trim();
  const ollamaKey = document.getElementById('set-ollama-key').value.trim();
  const settings = {
    provider: document.getElementById('set-provider').value,
    model: document.getElementById('set-model').value.trim(),
    effort: document.getElementById('set-effort').value,
    update_check_model: document.getElementById('set-upd-model').value.trim(),
    openrouter_api_key: orKey || null,
    anthropic_api_key: anKey || null,
    kilo_cli_user_profile: kiloProfile || null,
    kilo_cli_path: kiloPath || null,
    ollama_base_url: ollamaUrl,
    ollama_api_key: ollamaKey || null,
    confidence_threshold: numVal('set-conf', 80, 50, 95) / 100,
    decision_interval_secs: numVal('set-decint', 600, 10, 604800),
    event_log_poll_interval_secs: numVal('set-elpoll', 30, 5, 604800),
    wmi_poll_interval_secs: numVal('set-wmipoll', 300, 30, 604800),
    event_log_channels: splitList(document.getElementById('set-channels').value),
    log_directories: splitList(document.getElementById('set-dirs').value),
    game_mode_auto: document.getElementById('set-game-auto').checked,
    game_mode_power_boost: document.getElementById('set-game-power').checked,
  };
  const st = document.getElementById('set-status');

  const s = (lastStatus && lastStatus.settings) || {};
  const providerChanged = !!savedProvider && savedProvider !== settings.provider;
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
      st.textContent = 'Kilo CLI needs a model — e.g. kilo/minimax/minimax-m3 or kilo/anthropic/claude-sonnet-5';
      return;
    }
  }
  if (settings.provider === 'ollama') {
    if (!settings.model) {
      st.textContent = 'Ollama needs a model — e.g. llama3.2 or qwen2.5:7b (run `ollama pull` first)';
      return;
    }
  }

  st.textContent = 'Saving…';
  try {
    const result = await invoke('update_settings', { settings });
    st.textContent = commandMessage(result, 'Settings applied.');
    document.getElementById('set-or-key').value = '';
    document.getElementById('set-an-key').value = '';
    document.getElementById('set-ollama-key').value = '';
    document.getElementById('set-kilo-profile').value = '';
    document.getElementById('set-kilo-path').value = '';
    dirtyCards.delete('card-provider');
    savedProvider = settings.provider;
    if (providerChanged) {
      const advisorModel = document.getElementById('set-adv-model').value.trim();
      document.getElementById('set-adv-status').textContent = advisorModel
        ? 'Provider changed — save Advisor to apply its selected model.'
        : 'Provider changed — escalation model reset to the main model.';
      if (advisorModel) dirtyCards.add('card-advisor');
    }
    rememberModelValues(settings.provider);
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  }
}

// ── Advisor settings (apply live — no service restart) ───────────────────────

function fillAdvisorSettings(s) {
  if (!s) return;
  document.getElementById('set-adv-save').disabled = false;
  document.getElementById('set-adv-enabled').checked = !!s.enabled;
  document.getElementById('set-adv-model').value = s.escalation_model || '';
  document.getElementById('set-adv-effort').value = s.escalation_effort || '';
  document.getElementById('set-adv-conf').value = Math.round(
    (s.low_confidence_threshold != null ? s.low_confidence_threshold : 0.6) * 100
  );
}

async function saveAdvisorSettings() {
  const st = document.getElementById('set-adv-status');
  if (document.getElementById('set-provider').value !== savedProvider) {
    st.textContent = 'Save the AI provider first, then save Advisor.';
    return;
  }
  const settings = {
    enabled: document.getElementById('set-adv-enabled').checked,
    escalation_model: document.getElementById('set-adv-model').value.trim(),
    escalation_effort: document.getElementById('set-adv-effort').value,
    low_confidence_threshold: numVal('set-adv-conf', 60, 0, 95) / 100,
  };
  st.textContent = 'Saving…';
  try {
    const result = await invoke('set_advisor_settings', { settings });
    st.textContent = commandMessage(result, 'Advisor settings applied.');
    dirtyCards.delete('card-advisor');
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  }
}

// ── Updater settings (apply live — no service restart) ───────────────────────

const METHOD_BOXES = [['m-winget', 'winget'], ['m-choco', 'choco'], ['m-msstore', 'msstore']];

function fillUpdaterSettings(s) {
  if (!s) return;
  document.getElementById('set-upd-save').disabled = false;
  document.getElementById('set-upd-enabled').checked = !!s.enabled;
  // Seconds, matching the service's own range (it clamps to 300 … 365d). Showing
  // whole hours rounded a valid 5–59 minute schedule up to 1, and saving any other
  // updater option then wrote that back as 3600, silently changing the schedule.
  document.getElementById('set-upd-interval').value =
    Math.min(31536000, Math.max(300, s.schedule_interval_secs || 86400));
  const methods = s.methods || [];
  for (const [id, name] of METHOD_BOXES) document.getElementById(id).checked = methods.includes(name);
  document.getElementById('m-scoop').checked = false;
  document.getElementById('m-scoop').disabled = true;
  document.getElementById('set-native-enabled').checked = !!s.native_enabled;
  document.getElementById('set-sigpol').value = s.native_signature_policy || 'require_valid';
  const ignored = (s.ignored || []).slice().sort((a, b) => a.localeCompare(b));
  const wrap = document.getElementById('set-upd-ignored-wrap');
  wrap.hidden = ignored.length === 0;
  document.getElementById('set-upd-ignored').innerHTML = ignored.map((id) =>
    `<div class="set-ignored-row"><span>${esc(id)}</span><button class="upd-mini upd-ignore set-upd-unignore" data-id="${escAttr(id)}" type="button">Unignore</button></div>`
  ).join('');
}

document.getElementById('set-upd-ignored').addEventListener('click', async (e) => {
  const btn = e.target.closest('.set-upd-unignore');
  if (!btn) return;
  btn.disabled = true;
  try {
    const result = await invoke('set_app_ignore', { id: btn.dataset.id, ignore: false, note: '' });
    btn.closest('.set-ignored-row').remove();
    document.getElementById('set-upd-ignored-wrap').hidden =
      !document.getElementById('set-upd-ignored').children.length;
    toast(commandMessage(result, 'App unignored'), 'ok');
  } catch (err) {
    console.error('set_app_ignore failed', err);
    toast('Could not unignore app: ' + err, 'err');
    btn.disabled = false;
  }
});

async function saveUpdaterSettings() {
  const methods = METHOD_BOXES.filter(([id]) => document.getElementById(id).checked).map(([, n]) => n);
  const st = document.getElementById('set-upd-status');
  if (!methods.length) {
    st.textContent = 'Choose at least one update method.';
    return;
  }
  const settings = {
    enabled: document.getElementById('set-upd-enabled').checked,
    schedule_interval_secs: numVal('set-upd-interval', 86400, 300, 31536000),
    methods,
    native_enabled: document.getElementById('set-native-enabled').checked,
    native_signature_policy: document.getElementById('set-sigpol').value,
  };
  st.textContent = 'Saving…';
  try {
    const result = await invoke('set_updater_settings', { settings });
    st.textContent = commandMessage(result, 'Updater settings applied.');
    dirtyCards.delete('card-updater');
  } catch (e) {
    st.textContent = 'Failed: ' + e;
  }
}

// Disable a Settings save button while its save is in flight, so a double-click can't
// fire two saves. Previously this also guarded a service restart; now settings apply
// live, but the guard still prevents duplicate invocations.
function saveGuarded(btnId, fn) {
  const btn = document.getElementById(btnId);
  const card = btn.closest('.card');
  const fields = [...card.querySelectorAll('input:not(:disabled), select:not(:disabled), textarea:not(:disabled)')];
  btn.disabled = true;
  card.setAttribute('aria-busy', 'true');
  fields.forEach((field) => { field.disabled = true; });
  Promise.resolve().then(fn).finally(() => {
    fields.forEach((field) => { field.disabled = false; });
    card.removeAttribute('aria-busy');
    btn.disabled = false;
    if (SERVICE_SETTINGS_CARD_IDS.includes(card.id)) {
      setServiceSettingsCardLoading(card.id, !serviceSettingsCardAvailable(card.id));
    }
  });
}
document.getElementById('set-save').addEventListener('click', () => saveGuarded('set-save', saveSettings));
document.getElementById('test-provider').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  const st = document.getElementById('set-status');
  if (dirtyCards.has('card-provider')) {
    st.textContent = 'Save provider changes before testing them.';
    return;
  }
  btn.dataset.busy = '1';
  btn.disabled = true;
  btn.textContent = 'Testing…';
  st.textContent = 'Testing from the service account…';
  try {
    st.textContent = commandMessage(await invoke('test_provider'), 'Provider test passed.');
  } catch (err) {
    st.textContent = 'Test failed: ' + err;
  } finally {
    btn.dataset.busy = '';
    btn.textContent = 'Test provider';
    btn.disabled = !((lastStatus && lastStatus.capabilities) || []).includes('provider_test')
      || ['ServiceDisconnected', 'Connecting', 'Restarting'].includes(lastStatus && lastStatus.status);
  }
});
document.getElementById('set-adv-save').addEventListener('click', () => saveGuarded('set-adv-save', saveAdvisorSettings));
document.getElementById('set-upd-save').addEventListener('click', () => saveGuarded('set-upd-save', saveUpdaterSettings));
document.getElementById('set-autostart-save').addEventListener('click', () => saveGuarded('set-autostart-save', saveAutostartSetting));

// ── Pause ─────────────────────────────────────────────────────────────────────

document.getElementById('pause-btn').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  try {
    const result = await invoke('toggle_pause');
    toast(commandMessage(result, 'Pause state changed'), 'ok');
  }
  catch (e) { console.error('toggle_pause failed', e); toast('Could not change pause state: ' + e, 'err'); }
  finally { btn.disabled = false; }
  refresh();
});

document.getElementById('game-btn').addEventListener('click', async () => {
  // Manual toggle (a latch, distinct from the auto-detector's lease). If auto-detect has
  // it on, turning it off here is re-enabled by the next detector heartbeat (by design).
  const on = !(lastStatus && lastStatus.gaming);
  try {
    const result = await invoke('set_gaming', { on, manual: true });
    let msg = commandMessage(result, on ? 'Game Mode on' : 'Game Mode off');
    // A manual OFF only withdraws the current lease: while a fullscreen game is
    // running the detector re-asserts it within ~30s. Say so, and name the setting
    // that makes the choice stick, instead of confirming an off that won't hold.
    const autoOn = !!(lastStatus && lastStatus.settings && lastStatus.settings.game_mode_auto);
    if (!on && autoOn) {
      msg += ' — auto-detect can turn it back on while a fullscreen game runs. Disable Auto Game Mode in Settings for full manual control.';
    }
    toast(msg, 'ok');
  }
  catch (e) { console.error('set_gaming failed', e); toast('Could not change Game Mode: ' + e, 'err'); }
  refresh();
});

// ── About ─────────────────────────────────────────────────────────────────────

const aboutSvcEl = document.getElementById('about-service-version');
const aboutInstallBtn = document.getElementById('about-install-svc');
const aboutUpdatesBtn = document.getElementById('about-updates');
const aboutWarnEl = document.getElementById('about-version-warning');
const aboutDescription = document.getElementById('about-description');
const navUpdates = document.getElementById('nav-updates');
const updaterCard = document.getElementById('card-updater');
let aboutRequest = 0;

async function refreshAbout() {
  const request = ++aboutRequest;
  const [appVer, svcVer, svcState, portable] = await Promise.all([
    invoke('get_app_version').catch(() => null),
    invoke('get_service_version').catch(() => null),
    invoke('get_service_state').catch(() => 'unknown'),
    invoke('is_portable').catch(() => true),
  ]);
  if (request !== aboutRequest) return;
  document.getElementById('about-version').textContent = `Version ${appVer || '—'}`;

  let svcText = portable ? 'Portable service: ' : 'Service: ';
  if (svcState === 'not_installed') {
    svcText += 'not installed';
  } else if (svcVer) {
    svcText += svcVer;
  } else {
    svcText += svcState === 'running' ? 'running' : 'unknown';
  }
  if (svcState !== 'not_installed' && svcState !== 'running' && svcState !== 'unknown') {
    svcText += ` (${svcState})`;
  }
  aboutSvcEl.textContent = svcText;

  const installable = !portable && (svcState === 'not_installed' || svcState === 'unknown');
  runtimePortable = portable;
  const autostartCard = document.getElementById('card-autostart');
  autostartCard.hidden = portable;
  navUpdates.hidden = portable;
  updaterCard.hidden = portable;
  aboutDescription.textContent = portable
    ? 'Portable Eir monitors while this app is open and runs its service under your Windows account. Administrator-only repairs, continuous background monitoring, Start with Windows, and unattended app updates require the full installer.'
    : 'Eir is an autonomous Windows system guardian. A LocalSystem service watches the event log, application logs, services and security posture; an AI model diagnoses problems the moment they appear; reversible whitelisted fixes run automatically and anything disruptive waits here for your approval. It also keeps your installed apps up to date unattended, and learns this machine’s quirks as it goes.';
  console.assert(autostartCard.hidden === portable,
    'Portable mode must not expose installed-app startup controls');
  console.assert(navUpdates.hidden === portable && updaterCard.hidden === portable,
    'Portable mode must not expose privileged app-update controls');
  if (portable && document.getElementById('view-updates').classList.contains('active')) {
    showView('dashboard');
  }
  if (!portable && document.getElementById('view-settings').classList.contains('active')
      && !dirtyCards.has('card-autostart')) {
    fillAutostartSetting();
  }
  aboutInstallBtn.style.display = installable ? 'inline-block' : 'none';
  aboutUpdatesBtn.style.display = portable ? 'none' : 'inline-block';

  if (appVer && svcVer && appVer !== svcVer) {
    aboutWarnEl.textContent = `Warning: UI (${appVer}) and service (${svcVer}) versions differ — restart the service or reinstall Eir.`;
    aboutWarnEl.style.display = 'block';
  } else {
    aboutWarnEl.style.display = 'none';
  }
}

aboutInstallBtn.addEventListener('click', async () => {
  const st = document.getElementById('about-status');
  aboutInstallBtn.disabled = true;
  st.textContent = 'Installing service…';
  try {
    st.textContent = await invoke('install_service');
    await refreshAbout();
  } catch (e) {
    st.textContent = 'Install failed: ' + e;
  } finally {
    aboutInstallBtn.disabled = false;
  }
});

document.getElementById('about-github').addEventListener('click', () => {
  invoke('open_url', { url: 'https://github.com/Swatto86/eir' })
    .catch((e) => toast('Could not open GitHub: ' + e, 'err'));
});

aboutUpdatesBtn.addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  const st = document.getElementById('about-status');
  btn.disabled = true;
  st.textContent = 'Checking… if an update is found it will download and restart automatically (this can take a few minutes).';
  try {
    st.textContent = await invoke('check_updates_now');
  } catch (e) {
    st.textContent = 'Check failed: ' + e;
  } finally {
    btn.disabled = false;
  }
});

// Refresh service state every time the About view opens.
document.getElementById('nav').addEventListener('click', (e) => {
  const btn = e.target.closest('.nav-btn');
  if (btn && btn.dataset.view === 'about') refreshAbout();
});
refreshAbout();

// ── Boot ──────────────────────────────────────────────────────────────────────

refresh();
setInterval(refresh, 2000);

// Fetch the USD→GBP rate so costs display in pounds
invoke('gbp_per_usd').then((r) => { if (r > 0) gbpRate = r; }).catch(() => {});
