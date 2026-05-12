// ─── tiny helpers ──────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const show = (id) => $(id).classList.remove('hidden');
const hide = (id) => $(id).classList.add('hidden');

const VIEWS = ['view-landing', 'view-login', 'view-admin'];
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

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
  }[c]));
}

// ─── state ─────────────────────────────────────────────────────────────
let state = {
  view: 'landing',           // 'landing' | 'login' | 'admin'
  user: null,                // username when signed in
  pollHandle: null,
};

// ─── routing ───────────────────────────────────────────────────────────
async function route() {
  const hash = location.hash;
  if (hash === '#admin') {
    const me = await fetch('/api/me').then((r) => (r.ok ? r.json() : null));
    if (me) enterAdmin(me.user);
    else enterLogin();
  } else {
    enterLanding();
  }
}

function enterLanding() {
  state.view = 'landing';
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
  $('admin-user-label').textContent = user;
  showView('view-admin');
  startPolling(refreshAdmin);
}

function startPolling(fn) {
  stopPolling();
  fn();
  state.pollHandle = setInterval(fn, 3000);
}
function stopPolling() {
  if (state.pollHandle) { clearInterval(state.pollHandle); state.pollHandle = null; }
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
    $('footer-version').textContent = `v${stats.version}`;

    // Pull listener detail for each connected mount in parallel.
    const detail = await Promise.all(
      mounts.map(async (m) => {
        if (!m.source_connected) return { mount: m, listeners: [] };
        const slug = m.path.replace(/^\//, '');
        const r = await fetch(`/api/mounts/${encodeURIComponent(slug)}/listeners`);
        if (!r.ok) {
          if (r.status === 401) { enterLogin(); throw new Error('unauthorized'); }
          return { mount: m, listeners: [] };
        }
        const body = await r.json();
        return { mount: m, listeners: body.listener_ids };
      })
    );

    renderAdminMounts(detail);
  } catch (e) {
    console.error('admin refresh failed', e);
  }
}

function renderAdminMounts(rows) {
  const container = $('admin-mounts');
  if (rows.length === 0) {
    container.innerHTML = '<div class="streams-empty">NO MOUNTS CONFIGURED</div>';
    return;
  }
  container.innerHTML = rows.map(({ mount: m, listeners }) => `
    <div class="mount-card">
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
          <span class="mount-field-value ${m.source_connected ? 'live' : ''}">
            ${m.source_connected ? `● LIVE · ${fmtUptime(m.source_uptime_secs)}` : 'offline'}
          </span>
        </div>
        <div class="mount-field">
          <span class="mount-field-label">LISTENERS</span>
          <span class="mount-field-value">${m.listener_count}</span>
        </div>
        ${m.source_connected
          ? `<button class="btn btn-danger btn-sm" data-kick-source="${escapeHtml(m.path)}">KICK SOURCE</button>`
          : '<span></span>'}
      </div>
      ${m.source_connected ? renderListenersTable(listeners) : ''}
    </div>
  `).join('');

  // Wire up kick buttons after rendering.
  container.querySelectorAll('[data-kick-source]').forEach((btn) => {
    btn.addEventListener('click', () => kickSource(btn.dataset.kickSource));
  });
  container.querySelectorAll('[data-kick-listener]').forEach((btn) => {
    btn.addEventListener('click', () => kickListener(btn.dataset.kickListener));
  });
}

function renderListenersTable(ids) {
  if (ids.length === 0) {
    return '<div class="listeners-empty">no listeners connected</div>';
  }
  return `
    <table class="listeners-table">
      <thead>
        <tr><th>LISTENER ID</th><th></th></tr>
      </thead>
      <tbody>
        ${ids.map((id) => `
          <tr>
            <td>#${id}</td>
            <td style="text-align: right;">
              <button class="btn btn-danger btn-sm" data-kick-listener="${id}">KICK</button>
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
  refreshAdmin();
}

async function kickListener(id) {
  const r = await fetch(`/api/listeners/${id}`, { method: 'DELETE' });
  if (r.status === 401) { enterLogin(); return; }
  refreshAdmin();
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

$('logout-btn').addEventListener('click', async (ev) => {
  ev.preventDefault();
  await fetch('/api/logout', { method: 'POST' });
  state.user = null;
  location.hash = '';
  enterLanding();
});

// ─── boot ──────────────────────────────────────────────────────────────
window.addEventListener('hashchange', route);
route();
