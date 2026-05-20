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

// ─── toasts ────────────────────────────────────────────────────────────
// Top-right overlay notifications. Use for transient confirmations
// (saved, discarded). Sticky state info still belongs in the inline
// banner so it survives until acknowledged.
function pushToast(message, kind = 'success', ms = 2500) {
  const stack = $('toast-stack');
  if (!stack) return;
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = message;
  stack.appendChild(el);
  // Trigger the transition on the next frame so the initial offset state
  // is observed before the .in class lands.
  requestAnimationFrame(() => el.classList.add('in'));
  setTimeout(() => {
    el.classList.remove('in');
    setTimeout(() => el.remove(), 200);
  }, ms);
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
const CONFIG_SECTION_TITLES = {
  server: 'Server',
  transcode: 'Transcode',
  mounts: 'Mounts',
  autodjs: 'AutoDJs',
  relays: 'Relays',
  users: 'Users',
};
const configView = {
  section: 'server',
  snapshot: null,    // last server-confirmed values, for dirty tracking
  current: null,     // last GET /api/config response

  async enter(section) {
    this.section = CONFIG_SECTIONS.includes(section) ? section : 'server';
    this.setActiveNav();
    const titleEl = $('config-title');
    if (titleEl) titleEl.textContent = CONFIG_SECTION_TITLES[this.section] || 'Configuration';
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
    } else if (this.section === 'transcode') {
      this.renderTranscode();
    } else if (this.section === 'mounts') {
      this.renderMounts();
    } else {
      $('config-pane-body').innerHTML =
        `<div class="config-placeholder">— ${escapeHtml(this.section.toUpperCase())} editor coming soon —</div>`;
    }
  },

  renderServer() {
    const { server, logging, limits } = this.current;
    // `auth.source_password` is sent redacted on GET. The form starts blank
    // and the user only types here to *change* the password — blank means
    // "leave the running value alone".
    const hasGlobalSourcePw = !!(this.current.auth && this.current.auth.source_password);
    this.snapshot = this.snapshotFromCurrent();
    $('config-pane-body').innerHTML = `
      <form class="config-form" id="config-server-form" novalidate>
        <fieldset class="config-group">
          <legend class="config-group-title">BIND &amp; HOST</legend>
          ${this.field('stream_bind', 'STREAM BIND', server.stream_bind, { type: 'text', restart: true })}
          ${this.field('admin_bind',  'ADMIN BIND',  server.admin_bind,  { type: 'text', restart: true })}
          ${this.field('hostname',    'HOSTNAME',    server.hostname,    { type: 'text' })}
        </fieldset>

        <fieldset class="config-group">
          <legend class="config-group-title">LOGGING</legend>
          ${this.field('level',  'LEVEL',  logging.level,  { type: 'text' })}
          ${this.selectField('format', 'FORMAT', logging.format, ['pretty', 'json'])}
        </fieldset>

        <fieldset class="config-group">
          <legend class="config-group-title">LIMITS</legend>
          ${this.field('max_listeners_global',   'MAX LISTENERS',   limits.max_listeners_global,   { type: 'number', min: 1 })}
          ${this.field('ring_size',              'RING SIZE',       limits.ring_size,              { type: 'number', min: 1, restart: true,
            help: 'Per-mount broadcast buffer depth (slots). Each slot holds one stream packet. Larger values give slow listeners more time to catch up at the cost of memory.' })}
          ${this.field('slow_listener_grace_s',  'SLOW GRACE (s)',  limits.slow_listener_grace_s,  { type: 'number', min: 0,
            help: 'How long a listener may lag behind the live edge before being disconnected. Increase if your audience has unstable connections.' })}
          ${this.field('burst_size',             'BURST SIZE',      limits.burst_size,             { type: 'number', min: 0,
            help: 'Bytes of recent audio replayed to each new listener so players start playback instantly instead of waiting for live data. Icecast-compatible; 0 disables.' })}
          ${this.field('source_max_kbps',        'SOURCE MAX KBPS', limits.source_max_kbps ?? '',  { type: 'number', min: 0, hint: 'blank = unlimited' })}
        </fieldset>

        <fieldset class="config-group">
          <legend class="config-group-title">SOURCES</legend>
          ${this.field('source_password', 'GLOBAL SOURCE PW', '',
            { type: 'password',
              hint: hasGlobalSourcePw ? '(set — blank to keep)' : '(unset)',
              help: 'Optional shared password for source clients. Used both as the fallback for new [[mounts]] entries that don’t carry their own password and to authorize sources connecting on unlisted paths (dynamic mounts).' })}
        </fieldset>

        <div class="config-form-actions">
          <button type="button" class="btn btn-ghost"   id="config-server-discard" disabled>DISCARD</button>
          <button type="submit"  class="btn btn-primary" id="config-server-save"    disabled>SAVE CHANGES</button>
        </div>
      </form>
    `;
    this.bindServerForm();
  },

  snapshotFromCurrent() {
    const { server, logging, limits } = this.current;
    return {
      stream_bind: server.stream_bind,
      admin_bind: server.admin_bind,
      hostname: server.hostname,
      level: logging.level,
      format: logging.format,
      max_listeners_global: limits.max_listeners_global,
      ring_size: limits.ring_size,
      slow_listener_grace_s: limits.slow_listener_grace_s,
      burst_size: limits.burst_size,
      source_max_kbps: limits.source_max_kbps,
      // Password fields always start blank; dirty detection compares
      // against the blank snapshot, so typing anything flags as dirty.
      source_password: '',
    };
  },

  field(name, label, value, opts = {}) {
    const restart = opts.restart
      ? '<span class="restart-badge" title="Restart required for this change to take effect">RESTART</span>'
      : '';
    const hint = opts.hint ? `<span class="config-field-hint">${escapeHtml(opts.hint)}</span>` : '';
    const help = opts.help ? `<p class="config-field-help">${escapeHtml(opts.help)}</p>` : '';
    const min = opts.min != null ? ` min="${opts.min}"` : '';
    const idPrefix = opts.idPrefix || '';
    return `
      <div class="config-field" data-field="${name}">
        <div class="config-field-label">
          <label for="cf-${idPrefix}${name}">${label}</label>
          ${restart}
        </div>
        <input id="cf-${idPrefix}${name}" name="${name}" type="${opts.type}" value="${escapeHtml(String(value ?? ''))}"${min}>
        ${hint || '<span></span>'}
        ${help}
      </div>
    `;
  },

  selectField(name, label, value, options, idPrefix = '') {
    const opts = options
      .map((o) => `<option value="${o}"${o === value ? ' selected' : ''}>${o}</option>`)
      .join('');
    return `
      <div class="config-field" data-field="${name}">
        <label for="cf-${idPrefix}${name}">${label}</label>
        <select id="cf-${idPrefix}${name}" name="${name}">${opts}</select>
        <span></span>
      </div>
    `;
  },

  bindServerForm() {
    const form = $('config-server-form');
    const save = $('config-server-save');
    const discard = $('config-server-discard');

    const recomputeDirty = () => {
      const current = this.collectServerForm();
      const dirty = JSON.stringify(current) !== JSON.stringify(this.snapshot);
      save.disabled = !dirty;
      discard.disabled = !dirty;
    };

    form.addEventListener('input', recomputeDirty);
    form.addEventListener('change', recomputeDirty);

    discard.addEventListener('click', () => {
      this.renderServer();
      pushToast('Changes discarded.', 'success');
    });

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      this.clearFieldErrors();
      const collected = this.collectServerForm();
      const body = this.buildServerPutBody(collected);
      save.disabled = true;
      discard.disabled = true;
      try {
        const res = await fetch('/api/config/server', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        if (res.status === 401) { location.hash = ''; return; }
        const payload = await res.json().catch(() => null);
        if (res.status === 200) {
          this.snapshot = collected;
          if (payload) {
            this.current = {
              ...this.current,
              server: payload.server,
              logging: payload.logging,
              limits: payload.limits,
              path: payload.path,
              source: payload.source,
            };
          }
          save.disabled = true;
          discard.disabled = true;
          const warnings = payload?.applied_warnings || [];
          if (warnings.length) {
            // Warnings stick around — operator needs to remember to restart.
            this.showBanner(`Saved. ${warnings.join(' · ')}`, 'warning');
            pushToast('Configuration saved — restart required.', 'warning', 4000);
          } else {
            // No banner change; toast is the only feedback so layout stays put.
            pushToast('Configuration saved.', 'success');
          }
        } else if ((res.status === 400 || res.status === 422) && payload?.field) {
          this.markFieldError(payload.field, payload.error || 'invalid value');
          recomputeDirty();
        } else if (res.status === 500 && payload?.disk_written) {
          this.showBanner(
            'Saved to disk, but apply failed. Running server still uses the previous config; restart to load the new file.',
            'error',
          );
          this.snapshot = collected;
          save.disabled = true;
          discard.disabled = true;
        } else {
          const msg = payload?.error || res.statusText;
          this.showBanner(`Save failed: ${msg}`, 'error');
          recomputeDirty();
        }
      } catch (err) {
        this.showBanner(`Save failed: ${err.message}`, 'error');
        recomputeDirty();
      }
    });
  },

  collectServerForm() {
    const v = (id) => $(`cf-${id}`).value;
    const nOrNull = (id) => {
      const s = v(id).trim();
      return s === '' ? null : Number(s);
    };
    const n = (id) => Number(v(id));
    return {
      stream_bind: v('stream_bind').trim(),
      admin_bind: v('admin_bind').trim(),
      hostname: v('hostname').trim(),
      level: v('level').trim(),
      format: v('format'),
      max_listeners_global: n('max_listeners_global'),
      ring_size: n('ring_size'),
      slow_listener_grace_s: n('slow_listener_grace_s'),
      burst_size: n('burst_size'),
      source_max_kbps: nOrNull('source_max_kbps'),
      source_password: v('source_password'),
    };
  },

  buildServerPutBody(c) {
    const body = {
      server:  { stream_bind: c.stream_bind, admin_bind: c.admin_bind, hostname: c.hostname },
      logging: { level: c.level, format: c.format },
      limits:  {
        max_listeners_global: c.max_listeners_global,
        ring_size: c.ring_size,
        slow_listener_grace_s: c.slow_listener_grace_s,
        burst_size: c.burst_size,
        source_max_kbps: c.source_max_kbps,
      },
    };
    // Only include `auth` when the user actually typed a new password;
    // blank means "keep the running value alone", which on the wire is
    // simply leaving the auth key out.
    if (c.source_password && c.source_password.trim()) {
      body.auth = { source_password: c.source_password };
    }
    return body;
  },

  // ── transcode section ───────────────────────────────────────────────
  // Global [transcode] block. Optional: when the ENABLED checkbox is off,
  // the field values are kept locally for convenience but the save sends
  // `{ "transcode": null }` to clear the block.
  renderTranscode() {
    const tc = this.current.transcode || null;
    const enabled = tc !== null;
    // Defaults when the block doesn't exist yet — match config.toml's
    // typical seed values so the form is ready to enable.
    const format = (tc && tc.format) || 'mp3';
    const sample_rate = tc ? tc.sample_rate : 44100;
    const bitrate_kbps = tc ? tc.bitrate_kbps : 128;

    this.snapshot = { enabled, format, sample_rate, bitrate_kbps };

    $('config-pane-body').innerHTML = `
      <form class="config-form" id="config-transcode-form" novalidate>
        <fieldset class="config-group">
          <legend class="config-group-title">GLOBAL TRANSCODE</legend>

          <div class="config-field" data-field="enabled">
            <label for="cf-tc-enabled">ENABLED</label>
            <label class="toggle" for="cf-tc-enabled">
              <input id="cf-tc-enabled" name="enabled" type="checkbox"${enabled ? ' checked' : ''}>
              <span class="toggle-track"></span>
            </label>
            <span class="config-field-hint">Default applied to mounts/autodjs/relays without per-source overrides.</span>
          </div>

          ${this.selectField('format',       'FORMAT',      format,        ['mp3', 'vorbis'], 'tc-')}
          ${this.field('sample_rate',  'SAMPLE RATE', sample_rate,   { type: 'number', min: 1, hint: 'Hz', idPrefix: 'tc-' })}
          ${this.field('bitrate_kbps', 'BITRATE',     bitrate_kbps,  { type: 'number', min: 1, hint: 'kbps', idPrefix: 'tc-' })}
        </fieldset>

        <div class="config-form-actions">
          <button type="button" class="btn btn-ghost"   id="config-transcode-discard" disabled>DISCARD</button>
          <button type="submit"  class="btn btn-primary" id="config-transcode-save"    disabled>SAVE CHANGES</button>
        </div>
      </form>
    `;
    this.applyTranscodeEnabledState();
    this.bindTranscodeForm();
  },

  applyTranscodeEnabledState() {
    const enabled = $('cf-tc-enabled')?.checked ?? false;
    ['tc-format', 'tc-sample_rate', 'tc-bitrate_kbps'].forEach((id) => {
      const el = $(`cf-${id}`);
      if (el) el.disabled = !enabled;
    });
  },

  bindTranscodeForm() {
    const form = $('config-transcode-form');
    const save = $('config-transcode-save');
    const discard = $('config-transcode-discard');

    const recomputeDirty = () => {
      const current = this.collectTranscodeForm();
      // Sub-field changes only count as dirty when the block is enabled —
      // otherwise the body sends `null` and the in-memory field values are
      // ignored. This keeps the buttons quiet while editing-then-disabling.
      let dirty = current.enabled !== this.snapshot.enabled;
      if (!dirty && current.enabled) {
        dirty =
          current.format !== this.snapshot.format ||
          current.sample_rate !== this.snapshot.sample_rate ||
          current.bitrate_kbps !== this.snapshot.bitrate_kbps;
      }
      save.disabled = !dirty;
      discard.disabled = !dirty;
    };

    form.addEventListener('input', () => {
      this.applyTranscodeEnabledState();
      recomputeDirty();
    });
    form.addEventListener('change', () => {
      this.applyTranscodeEnabledState();
      recomputeDirty();
    });

    discard.addEventListener('click', () => {
      this.renderTranscode();
      pushToast('Changes discarded.', 'success');
    });

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      this.clearFieldErrors();
      const collected = this.collectTranscodeForm();
      const body = this.buildTranscodePutBody(collected);
      save.disabled = true;
      discard.disabled = true;
      try {
        const res = await fetch('/api/config/transcode', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        if (res.status === 401) { location.hash = ''; return; }
        const payload = await res.json().catch(() => null);
        if (res.status === 200) {
          this.snapshot = collected;
          if (payload) {
            this.current = {
              ...this.current,
              transcode: payload.transcode,
              path: payload.path,
              source: payload.source,
            };
          }
          save.disabled = true;
          discard.disabled = true;
          const warnings = payload?.applied_warnings || [];
          if (warnings.length) {
            this.showBanner(`Saved. ${warnings.join(' · ')}`, 'warning');
            pushToast('Configuration saved — restart required.', 'warning', 4000);
          } else {
            pushToast('Configuration saved.', 'success');
          }
        } else if ((res.status === 400 || res.status === 422) && payload?.field) {
          this.markFieldError(payload.field, payload.error || 'invalid value');
          recomputeDirty();
        } else if (res.status === 500 && payload?.disk_written) {
          this.showBanner(
            'Saved to disk, but apply failed. Running server still uses the previous config; restart to load the new file.',
            'error',
          );
          this.snapshot = collected;
          save.disabled = true;
          discard.disabled = true;
        } else {
          this.showBanner(`Save failed: ${payload?.error || res.statusText}`, 'error');
          recomputeDirty();
        }
      } catch (err) {
        this.showBanner(`Save failed: ${err.message}`, 'error');
        recomputeDirty();
      }
    });
  },

  collectTranscodeForm() {
    const enabled = $('cf-tc-enabled').checked;
    return {
      enabled,
      format: $('cf-tc-format').value,
      sample_rate: Number($('cf-tc-sample_rate').value),
      bitrate_kbps: Number($('cf-tc-bitrate_kbps').value),
    };
  },

  buildTranscodePutBody(c) {
    return {
      transcode: c.enabled
        ? { format: c.format, sample_rate: c.sample_rate, bitrate_kbps: c.bitrate_kbps }
        : null,
    };
  },

  clearFieldErrors() {
    document.querySelectorAll('.config-field-error').forEach((el) => el.remove());
    document.querySelectorAll('.config-field').forEach((el) => el.classList.remove('has-error'));
  },

  markFieldError(fieldPath, msg) {
    const leaf = fieldPath.includes('.') ? fieldPath.split('.').pop() : fieldPath;
    const wrap = document.querySelector(`.config-field[data-field="${CSS.escape(leaf)}"]`);
    if (!wrap) {
      this.showBanner(`Validation: ${msg} (${fieldPath})`, 'error');
      return;
    }
    wrap.classList.add('has-error');
    const err = document.createElement('span');
    err.className = 'config-field-error';
    err.textContent = msg;
    wrap.appendChild(err);
  },

  // ── mounts section ──────────────────────────────────────────────────
  // List view: all mounts collapsed to a one-line summary by default.
  // Click a row (or "+ ADD MOUNT") to expand the editor for exactly one
  // mount. Save sends only that mount's edits; the backend keeps the
  // other mounts' passwords by interpreting blank/omitted values as
  // "unchanged". The shared PUT endpoint receives the full list with
  // those mounts left as-is.
  renderMounts() {
    // In-memory working copy. Mutated as the user adds/removes/edits.
    this.mountsWorking = (this.current.mounts || []).map((m) => this.mountToWorking(m));
    this.mountsEditingIdx = null; // -1+ index, or null for "nothing open"
    this.drawMountsPane();
  },

  mountToWorking(m) {
    return {
      path: m.path || '',
      source_password: m.source_password === '***' ? '' : (m.source_password || ''),
      max_listeners: m.max_listeners ?? null,
      name: m.name ?? '',
      description: m.description ?? '',
      genre: m.genre ?? '',
      url: m.url ?? '',
      burst_size: m.burst_size ?? null,
      transcode: m.transcode
        ? { format: m.transcode.format, sample_rate: m.transcode.sample_rate, bitrate_kbps: m.transcode.bitrate_kbps }
        : null,
    };
  },

  drawMountsPane() {
    const rows = this.mountsWorking
      .map((m, idx) =>
        idx === this.mountsEditingIdx
          ? this.renderMountCardForm(m, idx)
          : this.renderMountCollapsedRow(m, idx),
      )
      .join('');
    const addingNew = this.mountsEditingIdx === this.mountsWorking.length - 1
      && this.mountsWorking.length > 0
      && this.mountsWorking[this.mountsEditingIdx].path === '';
    $('config-pane-body').innerHTML = `
      <div class="mounts-editor">
        <div class="mounts-list" id="mounts-list">
          ${rows || '<div class="config-placeholder">— no mounts configured —</div>'}
        </div>
        <div class="mounts-actions">
          <button type="button" class="btn btn-ghost" id="mounts-add"${this.mountsEditingIdx != null ? ' disabled' : ''}>
            + ADD MOUNT
          </button>
        </div>
      </div>
    `;
    this.bindMountsPane();
    void addingNew; // referenced only for clarity; nothing to do with it here.
  },

  renderMountCollapsedRow(m, idx) {
    const summary = [m.name, m.description].filter(Boolean).join(' · ');
    const tc = m.transcode
      ? `<span class="mount-row-meta">${escapeHtml(`${m.transcode.format} ${m.transcode.bitrate_kbps}k`)}</span>`
      : '';
    return `
      <div class="mount-row" data-action="edit" data-idx="${idx}" role="button" tabindex="0">
        <span class="mount-row-path">${escapeHtml(m.path || '(unnamed mount)')}</span>
        <span class="mount-row-summary">${escapeHtml(summary)}</span>
        ${tc}
        <span class="mount-row-edit">EDIT →</span>
      </div>
    `;
  },

  renderMountCardForm(m, idx) {
    const tc = m.transcode;
    const isNew = !this.current.mounts?.some((cur) => cur.path === m.path) || m.path === '';
    const passwordPlaceholder = isNew ? '(required, or leave blank for the global)' : '(unchanged if blank)';
    return `
      <fieldset class="mount-edit-card" data-mount-idx="${idx}">
        <legend class="mount-edit-card-title">
          <span class="mount-edit-card-index">#${idx + 1}</span>
          <span class="mount-edit-card-path">${escapeHtml(m.path || '(new mount)')}</span>
          <button type="button" class="btn btn-ghost mount-edit-card-remove" data-action="remove" data-idx="${idx}">REMOVE</button>
        </legend>

        ${mountTextField(idx, 'path', 'PATH', m.path, { required: true, placeholder: '/stream' })}
        ${mountTextField(idx, 'source_password', 'SOURCE PASSWORD', '',
            { type: 'password', placeholder: passwordPlaceholder })}
        ${mountTextField(idx, 'name', 'NAME', m.name)}
        ${mountTextField(idx, 'description', 'DESCRIPTION', m.description)}
        ${mountTextField(idx, 'genre', 'GENRE', m.genre)}
        ${mountTextField(idx, 'url', 'URL', m.url)}
        ${mountNumberField(idx, 'max_listeners', 'MAX LISTENERS', m.max_listeners,
            { min: 1, hint: 'blank = global' })}
        ${mountNumberField(idx, 'burst_size', 'BURST SIZE', m.burst_size,
            { min: 0, hint: 'blank = global' })}

        <div class="config-field" data-field="transcode_override">
          <div class="config-field-label">
            <label for="cf-m${idx}-tc-enabled">TRANSCODE OVERRIDE</label>
          </div>
          <label class="toggle" for="cf-m${idx}-tc-enabled">
            <input id="cf-m${idx}-tc-enabled" name="transcode_enabled" type="checkbox" data-mount-idx="${idx}" data-field-kind="tc-toggle"${tc ? ' checked' : ''}>
            <span class="toggle-track"></span>
          </label>
          <span class="config-field-hint">Overrides the global transcode for this mount only.</span>
        </div>

        <div class="mount-transcode" data-mount-idx="${idx}"${tc ? '' : ' hidden'}>
          ${mountSelectField(idx, 'tc_format', 'FORMAT', tc?.format ?? 'mp3', ['mp3', 'vorbis'])}
          ${mountNumberField(idx, 'tc_sample_rate', 'SAMPLE RATE', tc?.sample_rate ?? 44100, { min: 1, hint: 'Hz' })}
          ${mountNumberField(idx, 'tc_bitrate_kbps', 'BITRATE', tc?.bitrate_kbps ?? 128, { min: 1, hint: 'kbps' })}
        </div>

        <div class="config-form-actions">
          <button type="button" class="btn btn-ghost"   data-action="cancel-edit">CANCEL</button>
          <button type="button" class="btn btn-primary" data-action="save-edit">SAVE</button>
        </div>
      </fieldset>
    `;
  },

  bindMountsPane() {
    const list = $('mounts-list');
    const add = $('mounts-add');

    list.addEventListener('change', (e) => {
      // Toggle transcode-block visibility when its checkbox flips.
      if (e.target.matches('[data-field-kind="tc-toggle"]')) {
        const idx = e.target.dataset.mountIdx;
        const block = list.querySelector(`.mount-transcode[data-mount-idx="${idx}"]`);
        if (block) block.hidden = !e.target.checked;
      }
    });

    list.addEventListener('click', (e) => {
      const editBtn = e.target.closest('[data-action="edit"]');
      const removeBtn = e.target.closest('[data-action="remove"]');
      const saveBtn = e.target.closest('[data-action="save-edit"]');
      const cancelBtn = e.target.closest('[data-action="cancel-edit"]');

      if (editBtn) {
        if (this.mountsEditingIdx != null) return; // ignore while editing
        this.mountsEditingIdx = Number(editBtn.dataset.idx);
        this.drawMountsPane();
        const pathInput = $(`cf-m${this.mountsEditingIdx}-path`);
        if (pathInput) pathInput.focus();
        return;
      }
      if (removeBtn) {
        this.removeMountAtIndex(Number(removeBtn.dataset.idx));
        return;
      }
      if (cancelBtn) {
        // Discard any in-progress edits to this mount by re-reading from
        // the canonical state. New (unsaved) mounts are dropped entirely.
        const idx = this.mountsEditingIdx;
        const original = this.current.mounts?.[idx];
        if (original) {
          this.mountsWorking[idx] = this.mountToWorking(original);
        } else {
          this.mountsWorking.splice(idx, 1);
        }
        this.mountsEditingIdx = null;
        this.drawMountsPane();
        pushToast('Edit cancelled.', 'success');
        return;
      }
      if (saveBtn) {
        this.saveCurrentMountEdit();
        return;
      }
    });

    // Keyboard accessibility for the collapsed rows (role=button).
    list.addEventListener('keydown', (e) => {
      const row = e.target.closest('[data-action="edit"]');
      if (!row) return;
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        row.click();
      }
    });

    add.addEventListener('click', () => {
      if (this.mountsEditingIdx != null) return;
      this.mountsWorking.push({
        path: '',
        source_password: '',
        max_listeners: null,
        name: '',
        description: '',
        genre: '',
        url: '',
        burst_size: null,
        transcode: null,
      });
      this.mountsEditingIdx = this.mountsWorking.length - 1;
      this.drawMountsPane();
      const pathInput = $(`cf-m${this.mountsEditingIdx}-path`);
      if (pathInput) pathInput.focus();
    });
  },

  removeMountAtIndex(idx) {
    if (!confirm(`Remove mount "${this.mountsWorking[idx].path || `#${idx + 1}`}"?`)) return;
    // Build the post-remove list and send it to the server. The remaining
    // mounts have blank password fields, which the server resolves to the
    // existing per-mount passwords.
    const next = this.mountsWorking.slice();
    next.splice(idx, 1);
    this.persistMounts(next, { successMessage: 'Mount removed.' });
  },

  saveCurrentMountEdit() {
    const idx = this.mountsEditingIdx;
    if (idx == null) return;
    this.clearFieldErrors();
    // Read the form values for the edited mount into the working copy.
    this.mountsWorking[idx] = this.readMountFromDom(idx);
    // Client-side: path must be non-empty.
    if (!this.mountsWorking[idx].path) {
      this.markFieldError(`mounts[${idx}].path`, 'must be non-empty');
      return;
    }
    this.persistMounts(this.mountsWorking, { successMessage: 'Mount saved.' });
  },

  async persistMounts(workingList, { successMessage }) {
    const body = {
      mounts: workingList.map((m) => this.workingToPutBody(m)),
    };
    try {
      const res = await fetch('/api/config/mounts', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (res.status === 401) { location.hash = ''; return; }
      const payload = await res.json().catch(() => null);
      if (res.status === 200) {
        if (payload) {
          this.current = {
            ...this.current,
            mounts: payload.mounts,
            path: payload.path,
            source: payload.source,
          };
        }
        // Re-render from the canonical response so passwords are redacted
        // again and the collapsed list reflects on-disk state.
        this.renderMounts();
        const warnings = payload?.applied_warnings || [];
        if (warnings.length) {
          this.showBanner(`Saved. ${warnings.join(' · ')}`, 'warning');
          pushToast(`${successMessage} Restart required.`, 'warning', 4000);
        } else {
          pushToast(successMessage, 'success');
        }
      } else if ((res.status === 400 || res.status === 422) && payload?.field) {
        this.markFieldError(payload.field, payload.error || 'invalid value');
      } else if (res.status === 500 && payload?.disk_written) {
        this.showBanner(
          'Saved to disk, but apply failed. Running server still uses the previous config; restart to load the new file.',
          'error',
        );
      } else {
        this.showBanner(`Save failed: ${payload?.error || res.statusText}`, 'error');
      }
    } catch (err) {
      this.showBanner(`Save failed: ${err.message}`, 'error');
    }
  },

  readMountFromDom(idx) {
    const v = (k) => $(`cf-m${idx}-${k}`)?.value ?? '';
    const num = (k) => {
      const s = v(k).trim();
      return s === '' ? null : Number(s);
    };
    const tcEnabled = $(`cf-m${idx}-tc-enabled`)?.checked ?? false;
    return {
      path: v('path').trim(),
      source_password: v('source_password'),
      max_listeners: num('max_listeners'),
      name: v('name').trim(),
      description: v('description').trim(),
      genre: v('genre').trim(),
      url: v('url').trim(),
      burst_size: num('burst_size'),
      transcode: tcEnabled
        ? {
            format: v('tc_format') || 'mp3',
            sample_rate: Number(v('tc_sample_rate')) || 44100,
            bitrate_kbps: Number(v('tc_bitrate_kbps')) || 128,
          }
        : null,
    };
  },

  workingToPutBody(m) {
    const body = {
      path: m.path,
      source_password: m.source_password,
    };
    if (m.max_listeners != null) body.max_listeners = m.max_listeners;
    if (m.name) body.name = m.name;
    if (m.description) body.description = m.description;
    if (m.genre) body.genre = m.genre;
    if (m.url) body.url = m.url;
    if (m.burst_size != null) body.burst_size = m.burst_size;
    if (m.transcode) body.transcode = m.transcode;
    return body;
  },
};

// ─── mounts editor helpers (free functions, used by template literals) ───
function mountTextField(idx, name, label, value, opts = {}) {
  const type = opts.type || 'text';
  const placeholder = opts.placeholder ? ` placeholder="${escapeHtml(opts.placeholder)}"` : '';
  const required = opts.required ? ' required' : '';
  return `
    <div class="config-field" data-field="${name}">
      <div class="config-field-label">
        <label for="cf-m${idx}-${name}">${label}</label>
      </div>
      <input id="cf-m${idx}-${name}" name="${name}" type="${type}" value="${escapeHtml(String(value ?? ''))}"${placeholder}${required}>
      <span></span>
    </div>
  `;
}

function mountNumberField(idx, name, label, value, opts = {}) {
  const min = opts.min != null ? ` min="${opts.min}"` : '';
  const hint = opts.hint ? `<span class="config-field-hint">${escapeHtml(opts.hint)}</span>` : '<span></span>';
  return `
    <div class="config-field" data-field="${name}">
      <div class="config-field-label">
        <label for="cf-m${idx}-${name}">${label}</label>
      </div>
      <input id="cf-m${idx}-${name}" name="${name}" type="number" value="${escapeHtml(String(value ?? ''))}"${min}>
      ${hint}
    </div>
  `;
}

function mountSelectField(idx, name, label, value, options) {
  const opts = options
    .map((o) => `<option value="${o}"${o === value ? ' selected' : ''}>${o}</option>`)
    .join('');
  return `
    <div class="config-field" data-field="${name}">
      <div class="config-field-label">
        <label for="cf-m${idx}-${name}">${label}</label>
      </div>
      <select id="cf-m${idx}-${name}" name="${name}">${opts}</select>
      <span></span>
    </div>
  `;
}

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
    <div class="mount-edit-card" data-mount-edit-card="${escapeHtml(m.path)}" data-source-connected="${m.source_connected}">
      <div class="mount-edit-card-head">
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
  const cards = container.querySelectorAll('.mount-edit-card[data-mount-edit-card]');
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
