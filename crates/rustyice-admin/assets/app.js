// ─── tiny helpers ──────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const show = (id) => $(id).classList.remove('hidden');
const hide = (id) => $(id).classList.add('hidden');

const VIEWS = ['view-landing', 'view-login', 'view-admin', 'view-mount-detail'];
function showView(id) {
  for (const v of VIEWS) (v === id ? show : hide)(v);
}

function fmtUptime(secs) {
  if (secs == null) return '—';
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${s}s`;
  return `${s}s`;
}

// Format a bits-per-second rate with adaptive unit (bps / kbps / Mbps / Gbps).
function fmtBitrate(bytesPerSec) {
  if (bytesPerSec == null || !Number.isFinite(bytesPerSec) || bytesPerSec < 0) return '—';
  const bps = bytesPerSec * 8;
  if (bps < 1_000) return `${bps.toFixed(0)} bps`;
  if (bps < 1_000_000) return `${(bps / 1_000).toFixed(1)} kbps`;
  if (bps < 1_000_000_000) return `${(bps / 1_000_000).toFixed(2)} Mbps`;
  return `${(bps / 1_000_000_000).toFixed(2)} Gbps`;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
  }[c]));
}

// ─── state ─────────────────────────────────────────────────────────────
let state = {
  view: 'landing',           // 'landing' | 'login' | 'admin' | 'mount-detail'
  user: null,
  mountPath: null,           // set in mount-detail view
  pollHandle: null,
  bw: { in: null, out: null, ts: null },  // prior bandwidth sample for rate calc
};

// Compute and store an inbound/outbound rate snapshot from the latest
// cumulative byte counters in `stats`. Returns { rateIn, rateOut } in
// bytes-per-second, or `{ rateIn: null, rateOut: null }` on the first sample
// or after a counter reset (server restart).
function sampleBandwidth(stats) {
  const now = performance.now();
  const prev = state.bw;
  state.bw = { in: stats.total_bytes_in, out: stats.total_bytes_out, ts: now };
  if (prev.ts == null) return { rateIn: null, rateOut: null };
  const dt = (now - prev.ts) / 1000;
  if (dt <= 0) return { rateIn: null, rateOut: null };
  const dIn = stats.total_bytes_in - prev.in;
  const dOut = stats.total_bytes_out - prev.out;
  // Counter reset (server restart between polls): emit `null` rather than a
  // huge negative number.
  return {
    rateIn: dIn >= 0 ? dIn / dt : null,
    rateOut: dOut >= 0 ? dOut / dt : null,
  };
}

// ─── routing ───────────────────────────────────────────────────────────
async function route() {
  const hash = location.hash;
  const detailMatch = hash.match(/^#admin\/mount\/(.+)$/);
  if (hash === '#admin') {
    const me = await fetch('/api/me').then((r) => (r.ok ? r.json() : null));
    if (me) enterAdmin(me.user);
    else enterLogin();
  } else if (detailMatch) {
    const me = await fetch('/api/me').then((r) => (r.ok ? r.json() : null));
    if (!me) { enterLogin(); return; }
    let mountPath;
    try { mountPath = decodeURIComponent(detailMatch[1]); }
    catch { location.hash = '#admin'; return; }
    enterMountDetail(me.user, mountPath);
  } else {
    enterLanding();
  }
}

function enterLanding() {
  state.view = 'landing';
  state.mountPath = null;
  showView('view-landing');
  startPolling(refreshLanding);
}

function enterLogin() {
  state.view = 'login';
  showView('view-login');
  stopPolling();
  $('login-error').classList.add('hidden');
  $('login-user').focus();
}

function enterAdmin(user) {
  state.view = 'admin';
  state.user = user;
  state.mountPath = null;
  $('admin-user-label').textContent = user;
  showView('view-admin');
  startPolling(refreshAdmin);
}

function enterMountDetail(user, mountPath) {
  state.view = 'mount-detail';
  state.user = user;
  state.mountPath = mountPath;
  $('detail-user-label').textContent = user;
  $('detail-mount-path').textContent = mountPath;
  $('mount-detail').innerHTML = '';
  showView('view-mount-detail');
  startPolling(refreshMountDetail);
}

function startPolling(fn) {
  stopPolling();
  fn();
  state.pollHandle = setInterval(fn, 3000);
}
function stopPolling() {
  if (state.pollHandle) { clearInterval(state.pollHandle); state.pollHandle = null; }
}

function refreshCurrent() {
  if (state.view === 'admin') refreshAdmin();
  else if (state.view === 'mount-detail') refreshMountDetail();
  else if (state.view === 'landing') refreshLanding();
}

// ─── data: landing view (public read-only) ─────────────────────────────
async function refreshLanding() {
  try {
    const [mounts, stats] = await Promise.all([
      fetch('/api/mounts').then((r) => r.json()),
      fetch('/api/stats').then((r) => r.json()),
    ]);

    const live = mounts.filter((m) => m.source_connected);
    $('stat-listeners').textContent = stats.total_listeners;
    $('stat-mounts').textContent = live.length;
    $('stat-uptime').textContent = fmtUptime(stats.uptime_secs);
    $('streams-count').textContent = live.length;
    $('footer-version').textContent = `v${stats.version}`;

    const list = $('streams-list');
    if (live.length === 0) {
      list.innerHTML = '<div class="streams-empty">— no active streams —</div>';
      return;
    }
    const streamBase = `${location.protocol}//${location.hostname}:${stats.stream_port}`;
    list.innerHTML = live.map((m) => {
      const href = `${streamBase}${m.path}`;
      return `
        <a class="stream-row" href="${escapeHtml(href)}" target="_blank" rel="noopener">
          <div class="stream-meta">
            <span class="stream-path">${escapeHtml(m.path)}</span>
            ${m.name ? `<span class="stream-name">${escapeHtml(m.name)}</span>` : ''}
            ${m.title ? `<span class="stream-title">♪ ${escapeHtml(m.title)}</span>` : ''}
          </div>
          <span class="stream-listeners">
            <span class="live-dot"></span>${m.listener_count}
          </span>
        </a>
      `;
    }).join('');
  } catch (e) {
    console.error('landing refresh failed', e);
  }
}

// ─── data: admin view (auth required) ──────────────────────────────────
async function refreshAdmin() {
  try {
    const [mounts, stats] = await Promise.all([
      fetch('/api/mounts').then((r) => r.json()),
      fetch('/api/stats').then((r) => r.json()),
    ]);

    $('adm-stat-listeners').textContent = stats.total_listeners;
    $('adm-stat-mounts').textContent = mounts.filter((m) => m.source_connected).length;
    $('adm-stat-uptime').textContent = fmtUptime(stats.uptime_secs);
    const { rateIn, rateOut } = sampleBandwidth(stats);
    $('adm-stat-bw-in').textContent = fmtBitrate(rateIn);
    $('adm-stat-bw-out').textContent = fmtBitrate(rateOut);
    $('footer-version').textContent = `v${stats.version}`;

    const rows = mounts.map((m) => ({ mount: m, listeners: [] }));
    if (!updateMountListInPlace($('admin-mounts'), rows, { withListeners: false })) {
      renderMountList($('admin-mounts'), rows, { withListeners: false });
    }
  } catch (e) {
    console.error('admin refresh failed', e);
  }
}

// ─── data: mount detail view (auth required) ───────────────────────────
async function refreshMountDetail() {
  try {
    const slug = state.mountPath.replace(/^\//, '');
    const [mounts, stats, listenersResp] = await Promise.all([
      fetch('/api/mounts').then((r) => r.json()),
      fetch('/api/stats').then((r) => r.json()),
      fetch(`/api/mounts/${encodeURIComponent(slug)}/listeners`).then((r) => {
        if (r.status === 401) { enterLogin(); throw new Error('unauthorized'); }
        if (!r.ok) return null;
        return r.json();
      }),
    ]);

    $('footer-version').textContent = `v${stats.version}`;

    const mount = mounts.find((m) => m.path === state.mountPath);
    if (!mount) {
      $('mount-detail').innerHTML = '<div class="streams-empty">MOUNT NOT FOUND</div>';
      return;
    }
    const listeners = (listenersResp && listenersResp.listeners) || [];
    const rows = [{ mount, listeners }];
    if (!updateMountListInPlace($('mount-detail'), rows, { withListeners: true })) {
      renderMountList($('mount-detail'), rows, { withListeners: true });
    }
  } catch (e) {
    if (e && e.message !== 'unauthorized') console.error('mount detail refresh failed', e);
  }
}

// ─── mount card rendering ──────────────────────────────────────────────
function renderMountList(container, rows, opts) {
  if (rows.length === 0) {
    container.innerHTML = '<div class="streams-empty">NO MOUNTS CONFIGURED</div>';
    return;
  }
  container.innerHTML = rows.map((row) => renderMountCard(row, opts)).join('');
  bindMountListHandlers(container);
}

function renderMountCard({ mount: m, listeners }, opts) {
  const withListeners = !!(opts && opts.withListeners);
  const listenerCountHtml = withListeners
    ? `<span class="mount-field-value" data-listener-count>${m.listener_count}</span>`
    : `<a class="mount-field-value listener-link" href="#admin/mount/${encodeURIComponent(m.path)}">
         <span data-listener-count>${m.listener_count}</span>
         <span class="listener-link-cue">→</span>
       </a>`;
  return `
    <div class="mount-card" data-mount-card="${escapeHtml(m.path)}" data-source-connected="${m.source_connected}">
      <div class="mount-card-head">
        <div>
          <div class="mount-path">${escapeHtml(m.path)}</div>
          ${m.name ? `<div class="mount-name">${escapeHtml(m.name)}</div>` : ''}
        </div>
        <div class="mount-field">
          <span class="mount-field-label">CODEC</span>
          <span class="mount-field-value">${escapeHtml(m.codec)}</span>
        </div>
        <div class="mount-field">
          <span class="mount-field-label">SOURCE</span>
          <span class="mount-field-value ${m.source_connected ? 'live' : ''}" data-source-value>
            ${m.source_connected ? `● LIVE · ${fmtUptime(m.source_uptime_secs)}` : 'offline'}
          </span>
        </div>
        <div class="mount-field">
          <span class="mount-field-label">LISTENERS</span>
          ${listenerCountHtml}
        </div>
        ${m.source_connected
          ? `<button class="btn btn-danger btn-sm" data-kick-source="${escapeHtml(m.path)}">KICK SOURCE</button>`
          : '<span></span>'}
      </div>
      <div class="mount-title-row">
        <span class="mount-field-label">TITLE</span>
        <input
          type="text"
          class="mount-title-input"
          data-title-mount="${escapeHtml(m.path)}"
          value="${escapeHtml(m.title || '')}"
          placeholder="now playing — press SET to update"
          maxlength="256"
        >
        <button class="btn btn-primary btn-sm" data-set-title="${escapeHtml(m.path)}">SET</button>
        <button class="btn btn-ghost btn-sm" data-clear-title="${escapeHtml(m.path)}">CLEAR</button>
        <span class="mount-title-error error hidden" data-title-error="${escapeHtml(m.path)}"></span>
      </div>
      ${withListeners
        ? `<div class="mount-listeners" data-listeners-area>${renderListenersTable(listeners)}</div>`
        : ''}
    </div>
  `;
}

function bindMountListHandlers(container) {
  container.querySelectorAll('[data-kick-source]').forEach((btn) => {
    btn.addEventListener('click', () => kickSource(btn.dataset.kickSource));
  });
  bindKickListenerHandlers(container);
  container.querySelectorAll('[data-set-title]').forEach((btn) => {
    btn.addEventListener('click', () => setTitle(btn.dataset.setTitle));
  });
  container.querySelectorAll('[data-clear-title]').forEach((btn) => {
    btn.addEventListener('click', () => clearTitle(btn.dataset.clearTitle));
  });
  container.querySelectorAll('.mount-title-input').forEach((inp) => {
    inp.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        setTitle(inp.dataset.titleMount);
      }
    });
  });
}

function bindKickListenerHandlers(root) {
  root.querySelectorAll('[data-kick-listener]').forEach((btn) => {
    btn.addEventListener('click', () => kickListener(btn.dataset.kickListener));
  });
}

// Update existing mount cards in place. Returns false if the DOM structure
// no longer matches the incoming rows (mount added/removed, a source flipped
// between connected/offline, or the listeners area presence changed) —
// caller should perform a full rebuild.
function updateMountListInPlace(container, rows, opts) {
  const withListeners = !!(opts && opts.withListeners);
  const cards = container.querySelectorAll('.mount-card[data-mount-card]');
  if (cards.length !== rows.length) return false;
  for (let i = 0; i < rows.length; i++) {
    const card = cards[i];
    const { mount: m } = rows[i];
    if (card.dataset.mountCard !== m.path) return false;
    if (card.dataset.sourceConnected !== String(m.source_connected)) return false;
    const hasArea = !!card.querySelector('[data-listeners-area]');
    if (hasArea !== withListeners) return false;
  }
  for (let i = 0; i < rows.length; i++) {
    const card = cards[i];
    const { mount: m, listeners } = rows[i];

    if (m.source_connected) {
      const srcVal = card.querySelector('[data-source-value]');
      if (srcVal) srcVal.textContent = `● LIVE · ${fmtUptime(m.source_uptime_secs)}`;
    }

    const lcVal = card.querySelector('[data-listener-count]');
    if (lcVal) lcVal.textContent = String(m.listener_count);

    const input = card.querySelector('.mount-title-input');
    if (input && document.activeElement !== input) {
      const next = m.title || '';
      if (input.value !== next) input.value = next;
    }

    if (withListeners) {
      const area = card.querySelector('[data-listeners-area]');
      if (area) {
        const next = renderListenersTable(listeners);
        if (area.innerHTML !== next) {
          area.innerHTML = next;
          bindKickListenerHandlers(area);
        }
      }
    }
  }
  return true;
}

function renderListenersTable(listeners) {
  if (listeners.length === 0) {
    return '<div class="listeners-empty">no listeners connected</div>';
  }
  return `
    <table class="listeners-table">
      <thead>
        <tr><th>LISTENER ID</th><th>ADDRESS</th><th></th></tr>
      </thead>
      <tbody>
        ${listeners.map((l) => `
          <tr>
            <td>#${l.id}</td>
            <td class="listener-addr">${escapeHtml(l.address || '—')}</td>
            <td style="text-align: right;">
              <button class="btn btn-danger btn-sm" data-kick-listener="${l.id}">KICK</button>
            </td>
          </tr>
        `).join('')}
      </tbody>
    </table>
  `;
}

async function kickSource(path) {
  const slug = path.replace(/^\//, '');
  const r = await fetch(`/api/mounts/${encodeURIComponent(slug)}/source`, { method: 'DELETE' });
  if (r.status === 401) { enterLogin(); return; }
  refreshCurrent();
}

async function kickListener(id) {
  const r = await fetch(`/api/listeners/${id}`, { method: 'DELETE' });
  if (r.status === 401) { enterLogin(); return; }
  refreshCurrent();
}

async function setTitle(path) {
  const slug = path.replace(/^\//, '');
  const input = document.querySelector(`.mount-title-input[data-title-mount="${CSS.escape(path)}"]`);
  const errEl = document.querySelector(`[data-title-error="${CSS.escape(path)}"]`);
  errEl.classList.add('hidden');
  const title = input.value;
  const r = await fetch(`/api/mounts/${encodeURIComponent(slug)}/title`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ title }),
  });
  if (r.status === 401) { enterLogin(); return; }
  if (!r.ok) {
    const msg = await r.json().then((j) => j.message).catch(() => `Error ${r.status}`);
    errEl.textContent = msg;
    errEl.classList.remove('hidden');
    return;
  }
  refreshCurrent();
}

async function clearTitle(path) {
  const slug = path.replace(/^\//, '');
  const r = await fetch(`/api/mounts/${encodeURIComponent(slug)}/title`, { method: 'DELETE' });
  if (r.status === 401) { enterLogin(); return; }
  refreshCurrent();
}

// ─── login form ────────────────────────────────────────────────────────
$('login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const username = $('login-user').value.trim();
  const password = $('login-pass').value;
  const errEl = $('login-error');
  errEl.classList.add('hidden');
  try {
    const r = await fetch('/api/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, password }),
    });
    if (r.ok) {
      const me = await r.json();
      $('login-pass').value = '';
      location.hash = '#admin';
      enterAdmin(me.user);
    } else {
      errEl.textContent = r.status === 401 ? 'Invalid credentials' : `Sign-in failed (${r.status})`;
      errEl.classList.remove('hidden');
    }
  } catch (e) {
    errEl.textContent = 'Network error — try again.';
    errEl.classList.remove('hidden');
  }
});

$('login-cancel').addEventListener('click', (ev) => {
  ev.preventDefault();
  location.hash = '';
  enterLanding();
});

async function doLogout(ev) {
  ev.preventDefault();
  await fetch('/api/logout', { method: 'POST' });
  state.user = null;
  location.hash = '';
  enterLanding();
}
$('logout-btn').addEventListener('click', doLogout);
$('detail-logout-btn').addEventListener('click', doLogout);

// ─── boot ──────────────────────────────────────────────────────────────
window.addEventListener('hashchange', route);
route();
