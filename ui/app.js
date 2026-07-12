// Rusty frontend — vanilla JS. Talks to Rust via window.__TAURI__.core.invoke.
// No npm, no bundler. State lives in this file as plain objects.

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);

const state = {
  roots: [],          // ordered list of selected folder paths
  peeks: {},          // path -> FolderRow | null  (memory bank info)
  mode: 'dry',
  scanRunning: false,
  applyRunning: false,
  quarantineComplete: false,
  lastResponse: null,
  plan: [],
  logSince: 0,
  followLogs: true,
  paths: null,
  lastManifest: null,  // manifest path of the most recent apply, for undo
  compareFolders: [null, null],  // [folder1_path, folder2_path]
  compareRunning: false,
};

// ----------------------------- DOM helpers -----------------------------

const $ = (id) => document.getElementById(id);
const el = (tag, attrs = {}, ...children) => {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') node.className = v;
    else if (k === 'dataset') Object.assign(node.dataset, v);
    else if (k.startsWith('on') && typeof v === 'function')
      node.addEventListener(k.slice(2).toLowerCase(), v);
    else if (v === false || v == null) continue;
    else if (v === true) node.setAttribute(k, '');
    else node.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c == null) continue;
    node.appendChild(typeof c === 'string' ? document.createTextNode(c) : c);
  }
  return node;
};

const svgEl = (tag, attrs = {}) => {
  const node = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null) continue;
    node.setAttribute(k, v);
  }
  return node;
};

const fmtBytes = (n) => {
  if (n == null) return '–';
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let v = Number(n), u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return u === 0 ? `${v} ${units[u]}` : `${v.toFixed(2)} ${units[u]}`;
};
const fmtTs = (s) => s ? new Date(s).toLocaleString() : 'never';
const shortHash = (h) => h ? `${h.slice(0, 10)}…${h.slice(-4)}` : '';
const fileExt = (path) => {
  const base = path.split('/').pop() || '';
  const dot = base.lastIndexOf('.');
  return dot > 0 ? base.slice(dot + 1).toLowerCase() : '(no ext)';
};

// ----------------------------- folder pickers --------------------------

async function pickFolder() {
  try {
    const paths = await invoke('pick_folders');
    let added = false;
    for (const p of paths) {
      if (!state.roots.includes(p)) {
        state.roots.push(p);
        // Pre-load memory bank info so the UI shows scan history on the row.
        invoke('peek_folder', { path: p })
          .then((row) => { state.peeks[p] = row; renderRoots(); })
          .catch(() => {});
        added = true;
      }
    }
    if (added) renderRoots();
  } catch (err) {
    showError(err);
  }
}

function removeRoot(path) {
  state.roots = state.roots.filter((r) => r !== path);
  delete state.peeks[path];
  renderRoots();
}

function renderRoots() {
  const list = $('root-list');
  list.innerHTML = '';
  for (const root of state.roots) {
    const peek = state.peeks[root];
    const peekText = peek
      ? `seen ${fmtTs(peek.last_scan_ts)} · ${peek.last_file_count} files · ${fmtBytes(peek.last_total_bytes)}`
      : peek === null
        ? 'new folder — not in memory bank yet'
        : 'looking up…';
    const peekClass = peek ? 'known' : (peek === null ? 'new' : '');

    list.appendChild(
      el('li', {},
        el('div', { class: 'root-row' },
          el('span', { class: 'root-path' }, root),
          el('button', {
            class: 'root-remove',
            title: 'Remove',
            onClick: () => removeRoot(root),
          }, '×')
        ),
        el('span', { class: `root-peek ${peekClass}` }, peekText)
      )
    );
  }
}

// ----------------------------- mode toggle -----------------------------
// Single button: orange = dry, black = real-ready. Click to flip.

function setMode(mode) {
  state.mode = mode;
  const pill = document.getElementById('mode-seg');
  pill.classList.remove('dry', 'real');
  pill.classList.add(mode);
  document.body.classList.toggle('mode-real', mode === 'real');
  updatePlanBar();
}

// ----------------------------- scan ------------------------------------

async function runScan() {
  if (state.scanRunning) return;
  if (state.roots.length === 0) {
    showError({ message: 'Add at least one folder before scanning.' });
    return;
  }
  state.scanRunning = true;
  const btn = $('scan-btn');
  btn.disabled = true;
  btn.classList.add('scanning');
  btn.textContent = 'Scanning…';
  state.quarantineComplete = false;
  setCancelButton('scan');

  const scanMode = state.mode === 'real' ? 'real' : 'dry';
  setStatus(`Scanning ${state.roots.length} folder(s)…`, true);
  $('loading-text').textContent = `Scanning ${state.roots.length} folder(s)…`;
  resetScanProgress();
  $('m-folders').textContent = '0';
  $('m-files').textContent = '0';
  $('m-hashes').textContent = '0';
  $('m-lastscan').textContent = 'scanning';
  $('status-progress').hidden = false;
  $('loading-veil').hidden = false;

  let scanSucceeded = false;
  try {
    const minKb = parseInt($('min-size-kb').value, 10) || 0;
    const excludeRaw = $('exclude').value.trim();
    const exclude = excludeRaw
      ? excludeRaw.split(',').map((s) => s.trim()).filter(Boolean)
      : [];
    const resp = await invoke('run_scan', {
      request: {
        roots: state.roots,
        mode: scanMode,
        min_size: minKb > 0 ? minKb * 1024 : 0,
        skip_dev_dirs: $('skip-dev').checked,
        exclude,
        media_only: true,
      },
    });
    scanSucceeded = true;
    state.lastResponse = resp;
    state.plan = await invoke('get_default_plan');
    renderResults();
    renderPlan();
    renderDonut();
    $('m-folders').textContent = String((resp.counters.folders || {}).total_discovered ?? 0);
    $('m-files').textContent = String(resp.counters.hashes_reused + resp.counters.hashes_computed);
    $('m-hashes').textContent = String(resp.report.groups.length);
    $('export-btn').disabled = false;
    // renderPlan() already set apply-plan-btn's disabled state (real mode AND
    // something to quarantine); don't override it here.
    setStatus(`Done — ${resp.report.groups.length} duplicate group(s)`, false);

    // Refresh peek info for scanned folders.
    for (const p of state.roots) {
      invoke('peek_folder', { path: p })
        .then((row) => { state.peeks[p] = row; renderRoots(); })
        .catch(() => {});
    }
  } catch (err) {
    if (isCancelError(err)) {
      setStatus('Scan canceled.', false);
    } else {
      showError(err);
      setStatus('Idle', false);
    }
  } finally {
    state.scanRunning = false;
    setCancelButton('hidden');
    if (scanSucceeded) setScanProgress('done', 100, null);
    if (!scanSucceeded) $('status-progress').hidden = true;
    $('loading-veil').hidden = true;
    btn.classList.remove('scanning');
    if (scanSucceeded) {
      btn.classList.add('success');
      btn.textContent = 'Done ✓';
      setTimeout(() => {
        btn.classList.remove('success');
        btn.textContent = 'Scan';
        btn.disabled = false;
      }, 2500);
    } else {
      btn.textContent = 'Scan';
      btn.disabled = false;
    }
    await Promise.all([refreshMemoryStats(), refreshScans()]);
  }
}

function setStatus(text, running) {
  const s = $('status');
  if (!s) return;
  s.textContent = text;
  const done = !running && typeof text === 'string' && text.startsWith('Done');
  s.classList.toggle('running', !!running);
  s.classList.toggle('done', done);
}

function isCancelError(err) {
  return String(err?.message ?? err).toLowerCase().includes('cancel');
}

function setCancelButton(mode) {
  const btn = $('cancel-btn');
  btn.disabled = false;
  if (mode === 'scan') {
    btn.textContent = 'Cancel';
    btn.hidden = false;
  } else if (mode === 'apply') {
    btn.textContent = 'Cancel Remaining';
    btn.hidden = false;
  } else if (mode === 'clear') {
    btn.textContent = 'Clear';
    btn.hidden = false;
  } else {
    btn.textContent = 'Cancel';
    btn.hidden = true;
  }
}

function clearCompletedQuarantine() {
  state.quarantineComplete = false;
  state.lastResponse = null;
  state.plan = [];
  renderResults();
  renderPlan();
  renderDonut();
  $('export-btn').disabled = true;
  $('status-progress').hidden = true;
  setCancelButton('hidden');
  setStatus('Idle', false);
}

function resetScanProgress() {
  setScanProgress('hashing', 0, null);
}

const PHASE_LABELS = {
  inventory: 'Discovering folders & files…',
  'checking-cache': 'Checking memory bank…',
  hashing: null, // show percent
  saving: 'Saving hashes…',
  done: null,
};

function setScanProgress(phase, percent, progress) {
  const pct = Math.max(0, Math.min(100, Number(percent) || 0));
  const phaseLabel = PHASE_LABELS[phase];
  const indeterminate = phaseLabel != null;
  const labelText = indeterminate ? phaseLabel : `${pct}%`;
  const total = Number(progress?.total) || 0;
  const hashesReused = Number(progress?.hashes_reused) || 0;
  const hashesComputed = Number(progress?.hashes_computed) || 0;
  const hashesDone = hashesReused + hashesComputed;
  const overlayFill = $('progress-fill');
  const overlayLabel = $('progress-pct');
  if (overlayFill && overlayLabel) {
    overlayFill.style.width = `${pct}%`;
    overlayLabel.textContent = labelText;
  }
  // Surface the current phase as the loading caption too, so the app never just
  // says "scanning folder" with no sense of what stage it's in.
  const loadingText = $('loading-text');
  if (loadingText) {
    if (phase === 'hashing' && total > 0) {
      loadingText.textContent = `Hashing ${hashesDone} / ${total} file hashes…`;
    } else if (phaseLabel) {
      loadingText.textContent = phaseLabel;
    }
  }
  const statusFill = $('status-progress-fill');
  const statusLabel = $('status-progress-pct');
  if (statusFill && statusLabel) {
    statusFill.style.width = `${pct}%`;
    statusLabel.textContent = indeterminate ? `${pct}%` : `${pct}%`;
  }
}

function updateLiveMemoryStats(progress) {
  if (!progress || progress.phase === 'inventory') return;
  const total = Number(progress.total) || 0;
  const hashesDone = (Number(progress.hashes_reused) || 0) + (Number(progress.hashes_computed) || 0);
  $('m-files').textContent = total > 0 ? `${hashesDone} / ${total}` : String(hashesDone);
}

function renderResults() {
  const r = state.lastResponse;
  if (!r) {
    $('results-stats').textContent = 'No scan yet.';
    $('counters').innerHTML = '';
    $('files-empty').style.display = '';
    $('scan-summary').innerHTML = '';
    $('files-head').textContent = '';
    $('file-list').innerHTML = '';
    $('duplicates').innerHTML = '';
    $('dups-empty').style.display = '';
    updatePlanBar();
    return;
  }
  const c = r.counters;
  const f = c.folders || {};
  $('files-empty').style.display = 'none';
  $('results-stats').textContent =
    `${f.total_discovered ?? 0} folders · ${c.files_walked} files · ${r.report.groups.length} duplicate group(s) · ${fmtBytes(r.report.total_wasted_bytes)} wasted`;

  const counters = $('counters');
  counters.innerHTML = '';
  counters.appendChild(el('div', {}, `Folders: ${f.total_discovered ?? 0}`));
  counters.appendChild(el('div', {}, `Files: ${c.files_walked}`));
  counters.appendChild(el('div', {}, `Reused: ${c.hashes_reused} · Hashed: ${c.hashes_computed}`));
  counters.appendChild(el('div', {}, `Errors: ${c.errors}`));

  renderScanSummary(r);
  renderFiles(r);
  renderDuplicates(r);
  updatePlanBar();
}

const folderName = (p) => (p || '').replace(/\/+$/, '').split('/').filter(Boolean).pop() || (p || '');

function summaryGrid(rows) {
  const grid = el('div', { class: 'summary-grid' });
  for (const [label, val] of rows) {
    grid.appendChild(
      el('div', { class: 'summary-row' },
        el('span', { class: 'summary-label' }, label),
        el('span', { class: 'summary-val' }, String(val ?? 0)))
    );
  }
  return grid;
}

// The scan summary makes the recursive-folder counts impossible to miss, shows
// each added folder's own counts, and the combined totals across them all.
function renderScanSummary(r) {
  const c = r.counters;
  const f = c.folders || {};
  const perFolder = c.per_folder || [];
  const multi = perFolder.length > 1;

  const box = $('scan-summary');
  box.innerHTML = '';

  // ── Combined totals (or the single-folder summary) ──
  box.appendChild(el('div', { class: 'summary-title' }, multi ? 'Combined totals' : 'Scan summary'));
  box.appendChild(summaryGrid([
    ['Selected folders', f.selected_roots],
    ['Top-level folders', f.top_level],
    ['Nested folders', f.nested],
    ['Total folders discovered', f.total_discovered],
    ['Total folders scanned', f.scanned],
    ['Folders pruned (dev/cache/system)', f.pruned],
    ['Folders skipped (read errors)', f.skipped_errors],
    ['Empty folders', f.empty],
    ['Folders with supported files', f.with_supported],
    ['Folders with no supported files', f.without_supported],
    ['Folders with photos', f.with_photos],
    ['Folders with videos', f.with_videos],
    ['Folders with both', f.with_both],
    ['Supported files', c.files_walked],
    ['— Photos', c.photos],
    ['— Videos', c.videos],
    ['Unsupported files ignored', c.unsupported_files],
    ['Filtered files (size / exclude / system)', c.files_skipped],
    ['Hash cache hits', c.cache_hits],
    ['Hash cache misses', c.cache_misses],
    ['Stale hash records ignored', c.stale_ignored],
    ['New hashes saved', c.new_hashes_saved],
    ['Moved-file matches reused', c.moved_reused],
    ['Hash errors', c.errors],
  ]));

  // ── Per-folder breakdown (only when more than one folder was added) ──
  if (multi) {
    box.appendChild(el('div', { class: 'summary-title per-folder-heading' }, 'Per folder'));
    for (const p of perFolder) {
      const pf = p.folders || {};
      const block = el('div', { class: 'per-folder-block' });
      block.appendChild(el('div', { class: 'per-folder-name', title: p.root_display }, folderName(p.root_display)));
      block.appendChild(summaryGrid([
        ['Top-level / nested', `${pf.top_level} / ${pf.nested}`],
        ['Folders discovered', pf.total_discovered],
        ['Folders scanned', pf.scanned],
        ['Pruned / read errors', `${pf.pruned} / ${pf.skipped_errors}`],
        ['Empty folders', pf.empty],
        ['With / without media', `${pf.with_supported} / ${pf.without_supported}`],
        ['Supported files', p.files_walked],
        ['Photos / videos', `${p.photos} / ${p.videos}`],
        ['Unsupported ignored', p.unsupported_files],
        ['Cache hits / misses', `${p.cache_hits} / ${p.cache_misses}`],
        ['Stale ignored / new saved', `${p.stale_ignored} / ${p.new_hashes_saved}`],
        ['Errors', p.errors],
      ]));
      box.appendChild(block);
    }
  }
}

function renderFiles(r) {
  const c = r.counters;
  const sample = c.sample_files || [];
  const multi = (c.per_folder || []).length > 1;
  $('files-head').textContent = c.files_walked
    ? `Showing ${Math.min(sample.length, c.files_walked)} of ${c.files_walked} supported files (${c.photos} photos · ${c.videos} videos)${multi ? ' across all added folders' : ''} — all from one recursive scan.`
    : 'No supported photos or videos were found in the scanned tree.';
  const list = $('file-list');
  list.innerHTML = '';
  for (const s of sample) {
    const from = s.source_root ? `from ${folderName(s.source_root)} · ` : '';
    list.appendChild(
      el('div', { class: 'file-row compact' },
        el('span', { class: `file-kind ${s.media_kind}` }, s.media_kind),
        el('div', {},
          el('div', { class: 'file-path' }, s.path),
          el('div', { class: 'file-meta' }, `${from}${fmtBytes(s.size)}`)))
    );
  }
  if (c.files_walked > sample.length) {
    list.appendChild(el('div', { class: 'file-more' }, `… and ${c.files_walked - sample.length} more not shown`));
  }
}

function renderDuplicates(r) {
  const groups = r.report.groups;
  $('dups-empty').style.display = groups.length ? 'none' : '';
  if (!groups.length) {
    $('dups-empty').querySelector('.empty-text').textContent = r.counters.files_walked
      ? 'No exact duplicates found in this scan.'
      : 'Add folders in the sidebar, then press Scan.';
  }
  const container = $('duplicates');
  container.innerHTML = '';
  groups.forEach((g, idx) => container.appendChild(renderGroup(g, idx)));
}



function renderDupDir(d) {
  const paths = el('div', { class: 'dup-dir-paths' });
  for (const p of d.dirs) paths.appendChild(el('div', { class: 'dup-dir-path' }, p));
  const wasted = d.total_bytes * (d.dirs.length - 1);
  return el('div', { class: 'dup-dir' },
    el('div', { class: 'dup-dir-head' },
      el('span', { class: 'dup-dir-title' },
        `${d.dirs.length} identical folders · ${d.file_count} files each`),
      el('span', { class: 'group-wasted' }, `${fmtBytes(wasted)} wasted`)
    ),
    paths
  );
}

function renderGroup(g, idx) {
  const body = el('div', { class: 'group-body' });
  for (const f of g.files) {
    const planEntry = state.plan.find((p) => p.normalized_path === f.normalized_path);
    const action = planEntry?.action ?? 'quarantine';
    body.appendChild(
      el('div', { class: 'file-row' },
        el('span', { class: `file-action ${action}` }, action.toUpperCase()),
        el('div', {},
          el('div', { class: 'file-path' }, f.path),
          el('div', { class: 'file-meta' },
            `${fmtBytes(f.size)} · source: ${f.source_root}${f.moved_from ? ` · moved from ${f.moved_from}` : ''}${f.reused_from_cache ? ' · cache hit' : ''}`
          )
        ),
        el('button', {
          class: 'file-toggle',
          onClick: () => togglePlanAction(f.normalized_path),
        }, action === 'keep' ? 'quarantine' : 'keep')
      )
    );
  }
  const header = el('div', { class: 'group-header', onClick: (e) => {
    if (e.target.closest('.file-toggle')) return;
    e.currentTarget.parentElement.classList.toggle('open');
  }},
    el('div', { class: 'group-title' },
      el('span', { class: 'group-hash' }, shortHash(g.hash)),
      `${(g.media_kind || 'media').toUpperCase()} · ${g.copies} copies · ${fmtBytes(g.size)}/each`
    ),
    el('span', { class: 'group-wasted' }, `${fmtBytes(g.wasted_bytes)} wasted`)
  );
  return el('div', { class: `group${idx < 5 ? ' open' : ''}` }, header, body);
}

// ----------------------------- donut -----------------------------------
// Wasted-byte composition of duplicate groups, sliced by file extension.

const DONUT_COLORS = [
  'hsl(28 58% 48%)',   // rust orange
  'hsl(222 45% 58%)',  // muted blue
  'hsl(32 52% 55%)',   // amber
  'hsl(8 45% 48%)',    // red rust
  'hsl(26 40% 62%)',   // soft peach
  'hsl(20 35% 32%)',   // dark rust
  'hsl(36 48% 42%)',   // ochre
  'hsl(0 0% 25%)',     // neutral
];

let compositionChart = null;

function renderDonut() {
  const r = state.lastResponse;
  const canvas = $('donut');
  const center = $('donut-center');
  const legend = $('legend');
  legend.innerHTML = '';

  if (compositionChart) {
    compositionChart.destroy();
    compositionChart = null;
  }

  if (!r || !r.report.groups.length) {
    center.innerHTML = '<span class="donut-big">–</span><span class="donut-small">wasted</span>';
    return;
  }

  // Aggregate wasted bytes per extension. Cap at top 7, rest → "other".
  const buckets = new Map();
  for (const g of r.report.groups) {
    for (const f of g.files.slice(1)) { // first is the keeper, rest are wasted
      const ext = fileExt(f.path);
      buckets.set(ext, (buckets.get(ext) || 0) + f.size);
    }
  }
  let entries = [...buckets.entries()].sort((a, b) => b[1] - a[1]);
  const TOP = 7;
  if (entries.length > TOP) {
    const otherTotal = entries.slice(TOP).reduce((acc, [, v]) => acc + v, 0);
    entries = [...entries.slice(0, TOP), ['other', otherTotal]];
  }

  const total = entries.reduce((acc, [, v]) => acc + v, 0);
  center.innerHTML = '';
  center.appendChild(el('span', { class: 'donut-big' }, fmtBytes(total)));
  center.appendChild(el('span', { class: 'donut-small' }, 'wasted'));

  if (total === 0) return;

  const labels = entries.map(e => e[0]);
  const data = entries.map(e => e[1]);
  const colors = entries.map((_, i) => DONUT_COLORS[i % DONUT_COLORS.length]);

  compositionChart = new Chart(canvas, {
    type: 'doughnut',
    data: {
      labels,
      datasets: [{
        data,
        backgroundColor: colors,
        borderWidth: 0,
        hoverOffset: 4
      }]
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      cutout: '75%',
      plugins: {
        legend: { display: false },
        tooltip: {
          callbacks: {
            label: function(context) {
              let label = context.label || '';
              if (label) label += ': ';
              if (context.raw !== null) {
                label += fmtBytes(context.raw);
                const pct = (context.raw / total * 100).toFixed(1);
                label += ` (${pct}%)`;
              }
              return label;
            }
          }
        }
      },
      animation: {
        animateScale: true,
        animateRotate: true
      }
    }
  });

  entries.forEach(([ext, bytes], i) => {
    legend.appendChild(
      el('li', {},
        el('span', { class: 'swatch', style: `background:${DONUT_COLORS[i % DONUT_COLORS.length]}` }),
        el('span', { class: 'legend-label' }, ext),
        el('span', { class: 'legend-value' }, fmtBytes(bytes))
      )
    );
  });
}

// ----------------------------- plan ------------------------------------

async function togglePlanAction(normalizedPath) {
  const current = state.plan.find((p) => p.normalized_path === normalizedPath);
  if (!current) return;
  const next = current.action === 'keep' ? 'quarantine' : 'keep';
  try {
    state.plan = await invoke('set_plan_action', {
      update: { normalized_path: normalizedPath, action: next },
    });
    renderResults();
    renderPlan();
  } catch (err) {
    showError(err);
  }
}

function renderPlan() {
  const plan = state.plan;
  const toKeep = plan.filter((p) => p.action === 'keep');
  const toQuar = plan.filter((p) => p.action === 'quarantine');
  $('plan-summary').textContent =
    plan.length === 0
      ? 'No plan yet. Run a scan first.'
      : `${toKeep.length} keep · ${toQuar.length} quarantine · ${fmtBytes(toQuar.reduce((a, b) => a + b.size, 0))} to free`;
  $('apply-plan-btn').disabled = state.mode !== 'real' || toQuar.length === 0;
  updatePlanBar();
}

function updatePlanBar() {
  const toQuar = state.plan.filter((p) => p.action === 'quarantine');
  $('plan-action-bar').hidden = state.mode !== 'real' || toQuar.length === 0;
}

async function cancelScan() {
  if (state.quarantineComplete && !state.scanRunning && !state.applyRunning) {
    clearCompletedQuarantine();
    return;
  }
  try {
    await invoke('cancel_scan');
    if (state.applyRunning) {
      setStatus('Canceling remaining quarantine actions…', true);
    } else {
      setStatus('Canceling scan…', true);
    }
  } catch (err) {
    showError(err);
  }
}

// ----------------------------- apply / confirm -------------------------

function openConfirm() {
  const toQuar = state.plan.filter((p) => p.action === 'quarantine');
  const totalBytes = toQuar.reduce((a, b) => a + b.size, 0);
  const summary = $('confirm-summary');
  summary.innerHTML = '';
  summary.appendChild(el('li', {}, `Quarantine: ${toQuar.length} files`));
  summary.appendChild(el('li', {}, `Free up: ${fmtBytes(totalBytes)}`));
  summary.appendChild(el('li', {}, `Destination: ~/Desktop/Quarantine`));
  summary.appendChild(el('li', {}, `Original paths are recorded in the manifest and quarantine log.`));
  $('confirm-input').value = '';
  $('confirm-yes').disabled = true;
  $('confirm-backdrop').hidden = false;
  setTimeout(() => $('confirm-input').focus(), 50);
}

function closeConfirm() {
  $('confirm-backdrop').hidden = true;
}

async function applyPlan() {
  state.applyRunning = true;
  state.quarantineComplete = false;
  setCancelButton('apply');
  try {
    closeConfirm();
    setStatus('Applying plan…', true);
    const result = await invoke('apply_plan', { request: { confirmed: true } });
    setStatus(
      result.canceled
        ? `Quarantine stopped — moved ${result.quarantined}, left ${result.not_processed} untouched`
        : `Quarantine complete — moved ${result.quarantined}, freed ${fmtBytes(result.bytes_freed)}`,
      false,
    );
    state.plan = [];
    state.lastResponse = null;
    // Remember the manifest so the run can be undone (files moved back).
    state.lastManifest = result.manifest_path;
    const undoBtn = $('undo-btn');
    undoBtn.hidden = false;
    undoBtn.disabled = false;
    renderResults();
    renderPlan();
    renderDonut();
    $('export-btn').disabled = true;
    await Promise.all([refreshMemoryStats(), refreshScans()]);
    state.quarantineComplete = true;
    setCancelButton('clear');
  } catch (err) {
    showError(err);
    setStatus('Idle', false);
    setCancelButton('hidden');
  } finally {
    state.applyRunning = false;
  }
}

async function undoLastRun() {
  if (!state.lastManifest) return;
  const btn = $('undo-btn');
  btn.disabled = true;
  try {
    setStatus('Undoing last quarantine…', true);
    const res = await invoke('undo_run', {
      request: { manifest_path: state.lastManifest },
    });
    setStatus(
      `Undo complete — restored ${res.restored}${res.failed ? `, ${res.failed} failed` : ''}`,
      false,
    );
    state.lastManifest = null;
    btn.hidden = true;
    await Promise.all([refreshMemoryStats(), refreshScans()]);
  } catch (err) {
    showError(err);
    setStatus('Idle', false);
    btn.disabled = false;
  }
}

// ----------------------------- remembered folders ----------------------
// The memory bank knows every folder ever scanned. This panel lets the user
// re-add one to the scan list (Add) or purge it from the bank (Forget).

async function toggleRemembered() {
  const panel = $('remembered');
  const btn = $('remembered-toggle');
  if (panel.hidden) {
    await renderRemembered();
    panel.hidden = false;
    btn.setAttribute('aria-expanded', 'true');
  } else {
    panel.hidden = true;
    btn.setAttribute('aria-expanded', 'false');
  }
}

async function renderRemembered() {
  const panel = $('remembered');
  panel.innerHTML = '';
  let rows;
  try {
    rows = await invoke('list_remembered_folders');
  } catch (err) {
    showError(err);
    return;
  }
  if (!rows.length) {
    panel.appendChild(el('div', { class: 'remembered-empty' }, 'Memory bank has no folders yet.'));
    return;
  }
  for (const row of rows) {
    const already = state.roots.includes(row.path);
    panel.appendChild(
      el('div', { class: 'remembered-row' },
        el('div', { class: 'remembered-info' },
          el('span', { class: 'remembered-path' }, row.path),
          el('span', { class: 'remembered-meta' },
            `seen ${fmtTs(row.last_scan_ts)} · ${row.last_file_count} files · ${fmtBytes(row.last_total_bytes)}`)
        ),
        el('div', { class: 'remembered-actions' },
          el('button', {
            class: 'btn small',
            disabled: already,
            title: already ? 'Already in scan list' : 'Add to scan list',
            onClick: () => addRememberedRoot(row.path),
          }, already ? 'Added' : 'Add'),
          el('button', {
            class: 'btn small ghost',
            title: 'Remove from memory bank',
            onClick: () => forgetRemembered(row.normalized_path),
          }, 'Forget')
        )
      )
    );
  }
}

function addRememberedRoot(path) {
  if (!state.roots.includes(path)) {
    state.roots.push(path);
    invoke('peek_folder', { path })
      .then((r) => { state.peeks[path] = r; renderRoots(); })
      .catch(() => {});
    renderRoots();
  }
  renderRemembered(); // refresh the Add/Added button states
}

async function forgetRemembered(normalizedPath) {
  try {
    await invoke('forget_folder', { request: { normalized_path: normalizedPath } });
    await Promise.all([renderRemembered(), refreshMemoryStats()]);
  } catch (err) {
    showError(err);
  }
}

// ----------------------------- logs ------------------------------------

async function pollLogs() {
  try {
    const tail = await invoke('get_logs', { since: state.logSince });
    if (tail.entries.length === 0) return;
    state.logSince = tail.total;
    const logs = $('logs');
    for (const entry of tail.entries) {
      const line = el('div', { class: `log-line ${entry.level}` },
        `[${entry.ts.slice(11, 19)}] ${entry.level.toUpperCase().padEnd(5)} ${entry.message}`);
      logs.appendChild(line);
    }
    if (state.followLogs) logs.scrollTop = logs.scrollHeight;
  } catch (err) {
    // logs poll is best-effort; never surface errors here
  }
}

// ----------------------------- memory & scans --------------------------

async function refreshMemoryStats() {
  try {
    const stats = await invoke('memory_stats');
    // After a scan, show the recursive folder count actually discovered; before
    // any scan this session, fall back to the count of remembered roots.
    const f = state.lastResponse?.counters?.folders;
    $('m-folders').textContent = f ? f.total_discovered : stats.folders;
    $('m-files').textContent = stats.files;
    $('m-hashes').textContent = stats.duplicate_hashes ?? 0;
    $('m-lastscan').textContent = fmtTs(stats.last_scan_ts);
  } catch (err) {
    // non-fatal
  }
}

async function refreshScans() {
  try {
    const rows = await invoke('recent_scans');
    const container = $('scans');
    if (!container) return;
    container.innerHTML = '';
    if (rows.length === 0) {
      container.appendChild(el('div', { class: 'empty' }, 'No scans yet.'));
      return;
    }
    for (const r of rows) {
      container.appendChild(
        el('div', { class: 'scan-row' },
          el('span', { class: `scan-mode ${r.mode}` }, r.mode.toUpperCase()),
          el('div', {},
            el('div', {}, `Started ${fmtTs(r.started_ts)} · ${r.duplicate_groups} groups`),
            el('div', { class: 'file-meta' },
              `${r.files_seen} files · ${fmtBytes(r.bytes_seen)} walked · ${fmtBytes(r.wasted_bytes)} wasted`)
          ),
          el('span', { class: 'file-meta' }, r.roots.length + ' root(s)')
        )
      );
    }
  } catch (err) { /* non-fatal */ }
}

async function refreshPaths() {
  try {
    const p = await invoke('data_paths');
    state.paths = p;
    const ul = $('paths');
    ul.innerHTML = '';
    const root = p.data_root;
    // Everything lives under data_root; show the rest relative to it so long
    // absolute paths don't create tall wrapped blocks. The full path stays in
    // the tooltip and is what the Reveal button opens.
    const rel = (v) => (v !== root && root && v.startsWith(root + '/')) ? v.slice(root.length + 1) : v;
    for (const [label, key] of [
      ['Data', 'data_root'],
      ['Memory', 'memory_db'],
      ['Logs', 'logs_dir'],
      ['Exports', 'exports_dir'],
      ['Quarantine', 'quarantine_dir'],
      ['Manifests', 'manifests_dir'],
    ]) {
      const value = p[key];
      ul.appendChild(
        el('li', { class: 'revealable' },
          el('div', { class: 'path-head' },
            el('span', { class: 'path-name' }, label),
            el('button', {
              class: 'path-reveal',
              title: 'Reveal in Finder',
              onClick: () => revealPath(value),
            }, 'Reveal')
          ),
          el('span', { class: 'path-value', title: value }, rel(value))
        )
      );
    }
  } catch (err) { /* non-fatal */ }
}

async function revealPath(path) {
  try {
    await invoke('reveal_path', { path });
  } catch (err) {
    showError(err);
  }
}

// ----------------------------- export ----------------------------------

async function exportReport() {
  if (!state.lastResponse) return;
  try {
    const result = await invoke('export_report', { request: { format: 'csv' } });
    setStatus(`Exported ${fmtBytes(result.bytes_written)} to ${result.path}`, false);
  } catch (err) {
    showError(err);
  }
}


// ----------------------------- compare folders --------------------------

function updateCompareButton() {
  const btn = $('compare-btn');
  btn.disabled = !state.compareFolders[0] || !state.compareFolders[1] || state.compareRunning;
}

function renderCompareZone(slot) {
  const zone = $(`compare-drop-${slot}`);
  const path = state.compareFolders[slot - 1];
  if (path) {
    zone.classList.add('filled');
    zone.innerHTML = '';
    zone.appendChild(el('span', { class: 'compare-drop-icon' }, '📁'));
    zone.appendChild(el('span', { class: 'compare-drop-label' }, `Folder ${slot}`));
    zone.appendChild(el('span', { class: 'compare-drop-path' }, path));
    zone.appendChild(
      el('button', {
        class: 'compare-drop-remove',
        onClick: (e) => {
          e.stopPropagation();
          state.compareFolders[slot - 1] = null;
          renderCompareZone(slot);
          updateCompareButton();
        },
      }, 'Remove')
    );
  } else {
    zone.classList.remove('filled');
    zone.innerHTML = '';
    zone.appendChild(el('span', { class: 'compare-drop-icon' }, '📁'));
    zone.appendChild(el('span', { class: 'compare-drop-label' }, `Folder ${slot}`));
    zone.appendChild(el('span', { class: 'compare-drop-hint' }, 'Click or drag a folder here'));
  }
}

async function pickCompareFolder(slot) {
  try {
    const paths = await invoke('pick_folders');
    if (paths.length > 0) {
      state.compareFolders[slot - 1] = paths[0];
      renderCompareZone(slot);
      updateCompareButton();
    }
  } catch (err) {
    showError(err);
  }
}

// Attempt to route a Tauri drag-drop event to a compare drop zone.
// Returns true if the drop was consumed (hit an empty compare zone).
// Only intercepts when the user has already started filling compare zones
// (one slot occupied, the other empty) — otherwise drops go to Sources.
function tryCompareZoneDrop(paths) {
  if (!paths || paths.length === 0) return false;
  // Only intercept when exactly one slot is filled (user is mid-comparison setup)
  const has1 = !!state.compareFolders[0];
  const has2 = !!state.compareFolders[1];
  if (has1 === has2) return false; // both empty or both filled — don't intercept

  const path = paths[0];
  if (!has1) {
    state.compareFolders[0] = path;
    renderCompareZone(1);
    updateCompareButton();
    return true;
  } else {
    state.compareFolders[1] = path;
    renderCompareZone(2);
    updateCompareButton();
    return true;
  }
}

async function runComparison() {
  const folder1 = state.compareFolders[0];
  const folder2 = state.compareFolders[1];
  if (!folder1 || !folder2) return;
  if (state.compareRunning) return;

  state.compareRunning = true;
  const btn = $('compare-btn');
  btn.disabled = true;
  btn.textContent = 'Comparing…';
  btn.classList.add('scanning');

  const results = $('duplicates');
  results.innerHTML = '';

  try {
    const resp = await invoke('run_scan', {
      request: {
        roots: [folder1, folder2],
        mode: 'dry',
        min_size: 0,
        skip_dev_dirs: true,
        exclude: [],
        media_only: false,  // compare all files, not just media
      },
    });

    const groups = resp.report.groups;

    if (groups.length === 0) {
      results.appendChild(
        el('div', { class: 'compare-empty' },
          el('span', { class: 'compare-empty-icon' }, '✓'),
          el('span', { class: 'compare-empty-title' }, 'No duplicates found'),
          el('span', { class: 'compare-empty-text' },
            `Scanned ${resp.counters.files_walked} files across both folders — all unique.`)
        )
      );
    } else {
      // Summary
      results.appendChild(
        el('div', { class: 'compare-summary' },
          `Found `,
          el('span', { class: 'compare-stat' }, `${groups.length} duplicate group(s)`),
          ` across ${resp.counters.files_walked} files — `,
          el('span', { class: 'compare-stat' }, fmtBytes(resp.report.total_wasted_bytes)),
          ` wasted.`
        )
      );
      // Render groups using existing renderer
      groups.forEach((g, idx) => results.appendChild(renderGroup(g, idx)));
    }
    // Switch to Duplicates tab to show results
    document.querySelectorAll('.tab').forEach(t => {
      t.classList.toggle('active', t.dataset.tab === 'duplicates');
    });
    document.querySelectorAll('.tab-panel').forEach(p => {
      p.classList.toggle('active', p.id === 'panel-duplicates');
    });
  } catch (err) {
    if (!isCancelError(err)) {
      showError(err);
      results.appendChild(
        el('div', { class: 'compare-empty' },
          el('span', { class: 'compare-empty-title' }, 'Comparison failed'),
          el('span', { class: 'compare-empty-text' }, err?.message ?? String(err))
        )
      );
      // Switch to Duplicates tab to show the error
      document.querySelectorAll('.tab').forEach(t => {
        t.classList.toggle('active', t.dataset.tab === 'duplicates');
      });
      document.querySelectorAll('.tab-panel').forEach(p => {
        p.classList.toggle('active', p.id === 'panel-duplicates');
      });
    }
  } finally {
    state.compareRunning = false;
    btn.classList.remove('scanning');
    btn.textContent = 'Compare Folders';
    updateCompareButton();
  }
}

// ----------------------------- tabs ------------------------------------

const TAB_TITLES = { files: 'Files', duplicates: 'Duplicates', logs: 'Logs' };

function setupTabs() {
  document.querySelectorAll('.tab').forEach((tab) => {
    tab.addEventListener('click', () => {
      const updateDOM = () => {
        document.querySelectorAll('.tab').forEach((t) => t.classList.remove('active'));
        document.querySelectorAll('.tab-panel').forEach((p) => p.classList.remove('active'));
        tab.classList.add('active');
        $(`panel-${tab.dataset.tab}`).classList.add('active');
        // The Export button lives inside the Logs panel, so it shows only when
        // Logs is the active tab and is hidden otherwise — no extra JS needed.
        const title = document.querySelector('.content-title');
        if (title) title.textContent = TAB_TITLES[tab.dataset.tab] || 'Results';
      };

      if (!document.startViewTransition) {
        updateDOM();
      } else {
        document.startViewTransition(updateDOM);
      }
    });
  });
}

// ----------------------------- errors ----------------------------------

function showError(err) {
  const msg = err?.message ?? String(err);
  const logs = $('logs');
  logs.appendChild(el('div', { class: 'log-line error' }, `[ui] ERROR ${msg}`));
  logs.scrollTop = logs.scrollHeight;
  console.error(err);
}

// ----------------------------- about / updates -------------------------

function closeAbout() {
  const backdrop = $('about-backdrop');
  if (backdrop) backdrop.hidden = true;
}

async function openAbout() {
  const backdrop = $('about-backdrop');
  const body = $('about-body');
  const status = $('about-status');
  if (!backdrop || !body) return;
  status.hidden = true;
  status.textContent = '';
  status.className = 'about-status';
  body.innerHTML = '';
  try {
    const info = await invoke('get_app_info');
    const rows = [
      ['Name', info.name],
      ['Version', info.version],
      ['License', info.license],
      ['Organization', info.organization],
      ['Architecture', info.architecture],
    ];
    for (const [k, v] of rows) {
      body.appendChild(el('dt', {}, k));
      body.appendChild(el('dd', {}, String(v ?? '')));
    }
    $('about-title').textContent = `About ${info.name}`;
  } catch (err) {
    body.appendChild(el('dt', {}, 'Error'));
    body.appendChild(el('dd', {}, err?.message ?? String(err)));
  }
  backdrop.hidden = false;
}

async function checkForUpdates() {
  const backdrop = $('about-backdrop');
  const status = $('about-status');
  if (backdrop && backdrop.hidden) await openAbout();
  if (!status) return;
  status.hidden = false;
  status.className = 'about-status';
  status.textContent = 'Checking for updates…';
  try {
    const result = await invoke('check_for_updates');
    if (result.error) {
      status.className = 'about-status error';
      status.textContent = `Update check failed: ${result.error}`;
      return;
    }
    if (result.update_available) {
      status.className = 'about-status update';
      const url = result.download_url ? ` — ${result.download_url}` : '';
      status.textContent = `Update available: ${result.latest_version} (you have ${result.current_version})${url}`;
    } else {
      status.className = 'about-status ok';
      status.textContent = `You're up to date (${result.current_version}).`;
    }
  } catch (err) {
    status.className = 'about-status error';
    status.textContent = `Update check failed: ${err?.message ?? String(err)}`;
  }
}

function setupBrandGestures() {
  const brand = $('brand');
  if (!brand) return;
  brand.addEventListener('dblclick', (e) => {
    e.preventDefault();
    e.stopPropagation();
    openAbout();
  });
  brand.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    e.stopPropagation();
    checkForUpdates();
  });
  $('about-close')?.addEventListener('click', closeAbout);
  $('about-backdrop')?.addEventListener('click', (e) => {
    if (e.target === $('about-backdrop')) closeAbout();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && $('about-backdrop') && !$('about-backdrop').hidden) {
      closeAbout();
    }
  });
}


// ----------------------------- sidebar enhancements ---------------------

function setSidebarMode(mode) {
  const isScan = mode === 'scan';
  document.querySelectorAll('#sidebar-mode-toggle .mode-btn').forEach(btn => {
    const active = btn.dataset.mode === mode;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-selected', String(active));
  });
  $('panel-sources').hidden = !isScan;
  $('panel-filters').hidden = !isScan;
  $('panel-compare').hidden = isScan;
}

function setupCollapsiblePanels() {
  document.querySelectorAll('.panel-head.collapsible').forEach(head => {
    const toggle = () => {
      const panel = head.closest('.panel');
      panel.classList.toggle('collapsed');
      const expanded = !panel.classList.contains('collapsed');
      head.setAttribute('aria-expanded', String(expanded));
    };
    head.addEventListener('click', toggle);
    head.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        toggle();
      }
    });
  });
}

// Window focus/blur tracking
window.addEventListener('focus', () => document.body.classList.remove('window-inactive'));
window.addEventListener('blur', () => document.body.classList.add('window-inactive'));

// ----------------------------- boot ------------------------------------

function setupConfirmInput() {
  const input = $('confirm-input');
  input.addEventListener('input', () => {
    const v = input.value.trim().toLowerCase();
    $('confirm-yes').disabled = v !== 'y' && v !== 'yes';
    if (v === 'n' || v === 'no') {
      closeConfirm();
    }
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      const v = input.value.trim().toLowerCase();
      if (v === 'y' || v === 'yes') applyPlan();
      else if (v === 'n' || v === 'no') closeConfirm();
    } else if (e.key === 'Escape') {
      closeConfirm();
    }
  });
}

window.addEventListener('DOMContentLoaded', async () => {
  setupTabs();
  setupConfirmInput();
  setupBrandGestures();
  setupCollapsiblePanels();

  // Sidebar mode toggle click listeners
  document.querySelectorAll('#sidebar-mode-toggle .mode-btn').forEach(btn => {
    btn.addEventListener('click', () => setSidebarMode(btn.dataset.mode));
  });

  $('pick-folder').addEventListener('click', pickFolder);
  $('clear-folders').addEventListener('click', () => {
    state.roots = []; state.peeks = {}; renderRoots();
    if (!$('remembered').hidden) renderRemembered();
  });
  $('remembered-toggle').addEventListener('click', toggleRemembered);
  $('undo-btn').addEventListener('click', undoLastRun);
  document.querySelectorAll('#mode-seg .mode-opt').forEach((b) => {
    b.addEventListener('click', () => setMode(b.dataset.mode));
  });
  // Drag support: swipe the pill to switch mode (snap to whichever third released over)
  (function () {
    const pill = document.getElementById('mode-seg');
    const MODES = ['dry', 'real'];
    let startX = null;
    pill.addEventListener('pointerdown', (e) => { startX = e.clientX; });
    pill.addEventListener('pointerup', (e) => {
      if (startX === null) return;
      const dx = Math.abs(e.clientX - startX);
      startX = null;
      if (dx < 12) return; // small movement = click, let the button handle it
      const rect = pill.getBoundingClientRect();
      const idx = Math.max(0, Math.min(1, Math.floor(((e.clientX - rect.left) / rect.width) * 2)));
      setMode(MODES[idx]);
    });
    document.addEventListener('pointerup',     () => { startX = null; });
    document.addEventListener('pointercancel', () => { startX = null; });
  }());

  // OS file/folder drag-and-drop onto the app window → add to Sources
  if (window.__TAURI__?.event?.listen) {
    window.__TAURI__.event.listen('scan-progress', (ev) => {
      const p = ev.payload || {};
      setScanProgress(p.phase, p.percent, p);
      updateLiveMemoryStats(p);
      // Live recursive folder count — climbs during the walk, then holds.
      if (typeof p.folders === 'number' && p.folders > 0) {
        $('m-folders').textContent = String(p.folders);
      }
    });

    window.__TAURI__.event.listen('tauri://drag-enter', () => {
      document.body.classList.add('drag-over');
    });
    window.__TAURI__.event.listen('tauri://drag-leave', () => {
      document.body.classList.remove('drag-over');
    });
    window.__TAURI__.event.listen('tauri://drag-drop', (ev) => {
      document.body.classList.remove('drag-over');
      const paths = ev.payload?.paths || [];
      // Try to fill an empty compare zone first (sidebar-based, always available).
      if (tryCompareZoneDrop(paths)) return;
      let added = false;
      for (const p of paths) {
        if (!state.roots.includes(p)) {
          state.roots.push(p);
          invoke('peek_folder', { path: p })
            .then((row) => { state.peeks[p] = row; renderRoots(); })
            .catch(() => {});
          added = true;
        }
      }
      if (added) renderRoots();
    });
  }
  $('scan-btn').addEventListener('click', runScan);
  $('cancel-btn').addEventListener('click', cancelScan);
  $('export-btn').addEventListener('click', exportReport);
  $('apply-plan-btn').addEventListener('click', openConfirm);
  $('keeper-rule').addEventListener('change', async (e) => {
    if (!state.lastResponse) return;
    try {
      state.plan = await invoke('set_keeper_rule', { request: { rule: e.target.value } });
    renderResults();
    renderPlan();
    } catch (err) {
      showError(err);
    }
  });
  $('confirm-no').addEventListener('click', closeConfirm);
  $('confirm-yes').addEventListener('click', applyPlan);
  $('clear-logs').addEventListener('click', async () => {
    try { await invoke('clear_logs'); state.logSince = 0; $('logs').innerHTML = ''; } catch {}
  });
  $('follow').addEventListener('change', (e) => { state.followLogs = e.target.checked; });

  // Compare tab — click-to-browse on drop zones
  $('compare-drop-1').addEventListener('click', (e) => {
    if (e.target.closest('.compare-drop-remove')) return;
    if (!state.compareFolders[0]) pickCompareFolder(1);
  });
  $('compare-drop-2').addEventListener('click', (e) => {
    if (e.target.closest('.compare-drop-remove')) return;
    if (!state.compareFolders[1]) pickCompareFolder(2);
  });
  $('compare-btn').addEventListener('click', runComparison);

  renderRoots();
  setMode('dry');
  setStatus('Idle', false);
  renderDonut();

  await Promise.all([refreshMemoryStats(), refreshScans(), refreshPaths()]);

  // Try to load any prior results (e.g. previous app run completed but
  // user closed the window before exporting).
  try {
    const last = await invoke('get_last_results');
    if (last) {
      state.lastResponse = last;
      state.plan = await invoke('get_default_plan');
      renderResults();
      renderPlan();
      renderDonut();
      $('export-btn').disabled = false;
    }
  } catch {}

  // Guard: make sure the sidebar starts at the top (a focused form control can
  // otherwise scroll itself into view on first paint).
  const resetSidebar = () => {
    const sb = document.querySelector('.sidebar');
    if (sb) sb.scrollTop = 0;
    if (document.activeElement && document.activeElement !== document.body) {
      document.activeElement.blur();
    }
  };
  resetSidebar();
  requestAnimationFrame(resetSidebar);
  // Catch any late reflow (async stats/paths, window settling) that re-scrolls it.
  setTimeout(resetSidebar, 120);
  setTimeout(resetSidebar, 400);

  // Poll logs every 500ms. Cheap because Rust hands back only the delta.
  setInterval(pollLogs, 500);
});
