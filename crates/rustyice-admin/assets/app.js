// ─── tiny helpers ──────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const show = (id) => $(id).classList.remove('hidden');
const hide = (id) => $(id).classList.add('hidden');

const VIEWS = ['view-landing', 'view-login', 'view-admin', 'view-mount-detail', 'view-stream-detail', 'view-config'];
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

// ─── visualizer (real-time bars / oscilloscope) ────────────────────────
// Renders the live audio in the player block via the Web Audio API. Two
// modes: `bars` (frequency spectrum) and `line` (time-domain waveform).
// Falls back to synthetic motion if Web Audio is unavailable.
const BAR_COUNT = 32;
const viz = {
  bars: [],
  raf: null,
  ready: false,
  mode: 'bars',
  lineColor: '#4dabff',
  el: null, barsEl: null, canvas: null, ctx2d: null,
  actx: null, analyser: null, source: null,
  freqData: null, timeData: null, hasAudio: false,

  // Grab DOM nodes and build the bar elements once (idempotent).
  ensureEls() {
    if (this.ready) return;
    this.el = $('stream-viz');
    this.barsEl = $('viz-bars');
    this.canvas = $('viz-line');
    if (!this.el || !this.barsEl || !this.canvas) return;
    this.ctx2d = this.canvas.getContext('2d');
    this.lineColor = getComputedStyle(document.documentElement)
      .getPropertyValue('--accent').trim() || '#4dabff';
    for (let i = 0; i < BAR_COUNT; i++) {
      const b = document.createElement('span');
      b.className = 'viz-bar';
      b.style.transform = 'scaleY(0.05)';
      this.barsEl.appendChild(b);
      this.bars.push(b);
    }
    this.ready = true;
  },

  // Build the Web Audio graph once: media element → analyser → output.
  ensureAudio() {
    if (this.analyser) { this.hasAudio = true; return; }
    if (!window.AudioContext) { this.hasAudio = false; return; }
    try {
      this.actx = new AudioContext();
      this.source = this.actx.createMediaElementSource($('stream-audio'));
      this.analyser = this.actx.createAnalyser();
      this.analyser.fftSize = 256;
      this.source.connect(this.analyser);
      this.analyser.connect(this.actx.destination);
      this.freqData = new Uint8Array(this.analyser.frequencyBinCount);
      this.timeData = new Uint8Array(this.analyser.fftSize);
      this.hasAudio = true;
    } catch {
      this.hasAudio = false;   // tainted/unsupported — synthetic fallback
    }
  },

  setMode(mode) {
    if (mode !== 'bars' && mode !== 'line') return;
    this.ensureEls();
    if (!this.ready) return;
    this.mode = mode;
    this.barsEl.classList.toggle('hidden', mode !== 'bars');
    this.canvas.classList.toggle('hidden', mode !== 'line');
    for (const btn of document.querySelectorAll('#viz-toggle [data-viz-mode]')) {
      btn.classList.toggle('active', btn.dataset.vizMode === mode);
    }
    if (mode === 'bars') this.resetBars();
    else if (!this.raf) this.drawLine();   // paint a static frame when idle
  },

  start() {
    this.ensureEls();
    if (!this.ready) return;
    this.ensureAudio();
    if (this.actx && this.actx.state === 'suspended') this.actx.resume();
    if (this.raf) return;
    this.el.classList.add('active');
    const t0 = performance.now();
    const tick = (now) => {
      if (this.mode === 'line') this.drawLine();
      else this.drawBars((now - t0) / 1000);
      this.raf = requestAnimationFrame(tick);
    };
    this.raf = requestAnimationFrame(tick);
  },

  stop() {
    if (this.raf) { cancelAnimationFrame(this.raf); this.raf = null; }
    if (this.el) this.el.classList.remove('active');
    if (!this.ready) return;
    this.resetBars();
    if (this.mode === 'line') this.drawLine();   // settle to a flat line
  },

  resetBars() {
    for (const b of this.bars) b.style.transform = 'scaleY(0.05)';
  },

  drawBars(t) {
    if (this.hasAudio) {
      this.analyser.getByteFrequencyData(this.freqData);
      // Music energy sits low in the spectrum — map the bars over the
      // bottom three-quarters of the bins so the top isn't dead space.
      const per = Math.max(1, Math.floor((this.freqData.length * 0.75) / BAR_COUNT));
      for (let i = 0; i < BAR_COUNT; i++) {
        let sum = 0;
        for (let j = 0; j < per; j++) sum += this.freqData[i * per + j] || 0;
        const v = Math.max(0.05, sum / per / 255);
        this.bars[i].style.transform = `scaleY(${v.toFixed(3)})`;
      }
    } else {
      for (let i = 0; i < BAR_COUNT; i++) {
        const v = Math.max(0.06, 0.5 + 0.42 * Math.sin(t * 3 + i * 0.5));
        this.bars[i].style.transform = `scaleY(${v.toFixed(3)})`;
      }
    }
  },

  drawLine() {
    const cv = this.canvas, ctx = this.ctx2d;
    if (!cv || !ctx) return;
    const dpr = window.devicePixelRatio || 1;
    const w = cv.clientWidth, h = cv.clientHeight;
    if (!w || !h) return;
    if (cv.width !== Math.round(w * dpr) || cv.height !== Math.round(h * dpr)) {
      cv.width = Math.round(w * dpr);
      cv.height = Math.round(h * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    ctx.lineWidth = 2;
    ctx.lineJoin = 'round';
    ctx.strokeStyle = this.lineColor;
    ctx.beginPath();
    if (this.hasAudio && this.analyser) {
      this.analyser.getByteTimeDomainData(this.timeData);
      const len = this.timeData.length;
      for (let i = 0; i < len; i++) {
        const x = (i / (len - 1)) * w;
        const y = (this.timeData[i] / 255) * h;
        i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
      }
    } else {
      // flat baseline when there is no audio graph / nothing playing
      ctx.moveTo(0, h / 2);
      ctx.lineTo(w, h / 2);
    }
    ctx.stroke();
  },
};

// ─── stream player (public detail view) ────────────────────────────────
// Drives the hidden <audio> element on the stream-detail page. Owns all
// playback state so metadata polling can refresh freely without touching it.
const streamPlayer = {
  audio: null,
  url: null,
  playing: false,
  offline: false,

  // Lazily grab the <audio> element and wire its events exactly once.
  el() {
    if (!this.audio) {
      this.audio = $('stream-audio');
      this.audio.addEventListener('playing', () => this.setStatus('playing'));
      this.audio.addEventListener('waiting', () => {
        if (this.playing) this.setStatus('connecting…');
      });
      this.audio.addEventListener('error', () => {
        if (this.playing) {
          this.playing = false;
          this.render();
          this.setStatus('playback error');
          viz.stop();
        }
      });
    }
    return this.audio;
  },

  // Point the player at a mount's stream URL, resetting any prior playback.
  attach(url) {
    this.teardown();
    this.url = url;
    this.render();
    this.setStatus('ready');
  },

  // Reflect whether the mount is live. Offline disables the control.
  setOffline(offline) {
    this.offline = offline;
    if (offline && this.playing) this.stop();
    this.render();
    if (offline) this.setStatus('offline');
    else if (!this.playing) this.setStatus('ready');
  },

  toggle() {
    if (this.playing) this.stop();
    else this.play();
  },

  play() {
    if (!this.url || this.offline) return;
    const a = this.el();
    a.src = this.url;
    this.playing = true;
    this.render();
    this.setStatus('connecting…');
    viz.start();
    a.play().catch(() => {
      this.playing = false;
      this.render();
      this.setStatus('playback error');
      viz.stop();
    });
  },

  stop() {
    // Clear `playing` before tearing down `src` so the error/emptied event
    // fired by load() is ignored rather than shown as a failure.
    this.playing = false;
    const a = this.el();
    a.pause();
    a.removeAttribute('src');
    a.load();           // actually drop the Icecast connection, not just buffer
    this.render();
    this.setStatus('ready');
    viz.stop();
  },

  // Stop playback and forget the mount — called on every navigation.
  teardown() {
    this.playing = false;
    if (this.audio) {
      this.audio.pause();
      this.audio.removeAttribute('src');
      this.audio.load();
    }
    this.url = null;
    this.offline = false;
    viz.stop();
  },

  render() {
    const btn = $('stream-play-btn');
    if (!btn) return;
    btn.disabled = this.offline || !this.url;
    btn.classList.toggle('playing', this.playing);
    $('stream-play-icon').textContent = this.playing ? '◼' : '▶';
    $('stream-play-label').textContent = this.playing ? 'STOP' : 'PLAY';
  },

  setStatus(text) {
    const el = $('stream-player-status');
    if (el) el.textContent = text;
  },
};

// ─── config view ──────────────────────────────────────────────────────
const CONFIG_SECTIONS = ['server', 'transcode', 'autodjs', 'mounts', 'relays', 'users'];
const configView = {
  section: 'server',
  snapshot: null,    // last server-confirmed values, for dirty tracking
  current: null,     // last GET /api/config response

  async enter(section) {
    this.section = CONFIG_SECTIONS.includes(section) ? section : 'server';
    this.setActiveNav();
    this.clearBanner();
    $('config-pane-body').innerHTML = '<div class="config-placeholder">loading…</div>';
    try {
      const res = await fetch('/api/config');
      if (res.status === 401) { location.hash = ''; return; }
      if (!res.ok) throw new Error(`config fetch failed: ${res.status}`);
      this.current = await res.json();
      this.maybeShowDefaultsBanner(this.current);
      this.renderSection();
    } catch (e) {
      $('config-pane-body').innerHTML =
        `<div class="config-placeholder">failed to load config: ${escapeHtml(e.message)}</div>`;
    }
  },

  setActiveNav() {
    document.querySelectorAll('[data-config-section]').forEach((el) => {
      el.classList.toggle('active', el.dataset.configSection === this.section);
    });
  },

  clearBanner() {
    const b = $('config-banner');
    b.className = 'config-banner hidden';
    b.innerHTML = '';
  },

  showBanner(message, kind = 'warning') {
    const b = $('config-banner');
    b.className = `config-banner ${kind}`;
    b.textContent = message;
  },

  maybeShowDefaultsBanner(data) {
    if (data.source === 'defaults') {
      this.showBanner(
        'Running on built-in defaults. First save will create ./config.toml next to the running binary.',
      );
    }
  },

  renderSection() {
    if (this.section === 'server') {
      this.renderServer();
    } else {
      $('config-pane-body').innerHTML =
        `<div class="config-placeholder">— ${escapeHtml(this.section.toUpperCase())} editor coming soon —</div>`;
    }
  },

  renderServer() {
    // Implemented in Task 15.
    $('config-pane-body').innerHTML =
      '<div class="config-placeholder">server form rendering wired in Task 15</div>';
  },
};

// ─── routing ───────────────────────────────────────────────────────────
async function route() {
  // route() runs only on navigation (hashchange / boot), never on poll —
  // so tearing the player down here stops audio whenever the user leaves
  // the detail page (or switches to a different stream).
  streamPlayer.teardown();
  const hash = location.hash;
  const detailMatch = hash.match(/^#admin\/mount\/(.+)$/);
  const configMatch = hash.match(/^#admin\/config(?:\/(.+))?$/);
  const streamMatch = hash.match(/^#stream\/(.+)$/);
  if (hash === '#admin') {
    const me = await fetch('/api/me').then((r) => (r.ok ? r.json() : null));
    if (me) enterAdmin(me.user);
    else enterLogin();
  } else if (configMatch) {
    const me = await fetch('/api/me').then((r) => (r.ok ? r.json() : null));
    if (!me) { enterLogin(); return; }
    enterConfig(me.user, configMatch[1] || 'server');
  } else if (detailMatch) {
    const me = await fetch('/api/me').then((r) => (r.ok ? r.json() : null));
    if (!me) { enterLogin(); return; }
    let mountPath;
    try { mountPath = decodeURIComponent(detailMatch[1]); }
    catch { location.hash = '#admin'; return; }
    enterMountDetail(me.user, mountPath);
  } else if (streamMatch) {
    let mountPath;
    try { mountPath = decodeURIComponent(streamMatch[1]); }
    catch { location.hash = ''; return; }
    enterStreamDetail(mountPath);
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

function enterStreamDetail(mountPath) {
  state.view = 'stream-detail';
  state.mountPath = mountPath;
  $('stream-detail-title').textContent = mountPath;
  $('stream-meta').innerHTML = '';
  showView('view-stream-detail');
  startPolling(refreshStreamDetail);
}

function enterConfig(user, section) {
  state.view = 'config';
  state.user = user;
  state.mountPath = null;
  $('config-user-label').textContent = user;
  showView('view-config');
  stopPolling(); // config view is edit-driven, not poll-driven
  configView.enter(section);
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
  else if (state.view === 'stream-detail') refreshStreamDetail();
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
    list.innerHTML = live.map((m) => {
      const href = `#stream/${encodeURIComponent(m.path)}`;
      return `
        <a class="stream-row" href="${href}">
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

// ─── data: public stream detail view ──────────────────────────────────
async function refreshStreamDetail() {
  try {
    const [mounts, stats] = await Promise.all([
      fetch('/api/mounts').then((r) => r.json()),
      fetch('/api/stats').then((r) => r.json()),
    ]);
    $('footer-version').textContent = `v${stats.version}`;

    const mount = mounts.find((m) => m.path === state.mountPath);

    if (!mount || !mount.source_connected) {
      $('stream-detail-title').textContent =
        (mount && mount.name) || state.mountPath;
      $('stream-meta').innerHTML =
        '<div class="streams-empty">— stream offline —</div>';
      streamPlayer.setOffline(true);
      return;
    }

    $('stream-detail-title').textContent = mount.name || mount.path;

    const url =
      `${location.protocol}//${location.hostname}:${stats.stream_port}${mount.path}`;
    // attach() resets playback, so only call it when the URL actually
    // changes — otherwise the 3s poll would interrupt audio every tick.
    if (streamPlayer.url !== url) streamPlayer.attach(url);
    streamPlayer.setOffline(false);

    $('stream-meta').innerHTML = renderStreamCard(mount);
  } catch (e) {
    console.error('stream detail refresh failed', e);
  }
}

// Parse an ICE `audio_info` string ("samplerate=44100;channels=2;...") into
// a flat key/value object.
function parseAudioInfo(s) {
  const out = {};
  if (!s) return out;
  for (const part of s.split(';')) {
    const eq = part.indexOf('=');
    if (eq > 0) out[part.slice(0, eq).trim()] = part.slice(eq + 1).trim();
  }
  return out;
}

// One-line technical spec, e.g. "mp3 · 128 kbps · 44.1 kHz · stereo".
function streamSpec(m) {
  const ai = parseAudioInfo(m.audio_info);
  const parts = [m.codec];
  const br = m.bitrate_kbps || (ai.bitrate ? Number(ai.bitrate) : null);
  if (br) parts.push(`${br} kbps`);
  if (ai.samplerate) parts.push(`${Number(ai.samplerate) / 1000} kHz`);
  if (ai.channels) parts.push(Number(ai.channels) === 1 ? 'mono' : 'stereo');
  return parts.join(' · ');
}

function renderStreamCard(m) {
  const desc = m.description
    ? `<div class="np-desc">${escapeHtml(m.description)}</div>` : '';
  const chip = m.genre
    ? `<span class="np-chip">${escapeHtml(m.genre)}</span>` : '';
  return `
    <div class="np-card">
      <div class="np-label">NOW PLAYING</div>
      <div class="np-track">${escapeHtml(m.title || '—')}</div>
      ${desc}
      <div class="np-tags">
        ${chip}
        <span class="np-spec">${escapeHtml(streamSpec(m))}</span>
      </div>
      <div class="np-stats">
        <span><strong>${m.listener_count}</strong> LISTENERS</span>
        <span class="np-sep">·</span>
        <span>UP <strong>${escapeHtml(fmtUptime(m.source_uptime_secs))}</strong></span>
      </div>
    </div>
  `;
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
$('config-logout-btn').addEventListener('click', doLogout);
$('stream-play-btn').addEventListener('click', () => streamPlayer.toggle());
$('viz-toggle').addEventListener('click', (e) => {
  const btn = e.target.closest('[data-viz-mode]');
  if (btn) viz.setMode(btn.dataset.vizMode);
});

// ─── boot ──────────────────────────────────────────────────────────────
window.addEventListener('hashchange', route);
route();
