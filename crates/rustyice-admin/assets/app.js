async function load() {
  const [mounts, stats] = await Promise.all([
    fetch('/api/mounts').then(r => r.json()),
    fetch('/api/stats').then(r => r.json()),
  ]);

  document.getElementById('uptime').textContent = stats.uptime_secs + 's';
  document.getElementById('listeners').textContent = stats.total_listeners;

  const tbody = document.getElementById('mounts-body');
  tbody.innerHTML = '';
  for (const m of mounts) {
    const tr = document.createElement('tr');
    tr.innerHTML = `
      <td>${m.path}</td>
      <td>${m.codec}</td>
      <td>${m.source_connected ? '✓' : '—'}</td>
      <td>${m.listener_count}</td>
      <td>${m.source_uptime_secs != null ? m.source_uptime_secs + 's' : '—'}</td>
      <td>${m.source_connected
        ? `<button onclick="kickSource('${m.path}')">Kick source</button>`
        : ''}</td>
    `;
    tbody.appendChild(tr);
  }
}

async function kickSource(path) {
  await fetch('/api/mounts/' + encodeURIComponent(path.slice(1)) + '/source', { method: 'DELETE' });
  load();
}

load();
setInterval(load, 5000);
