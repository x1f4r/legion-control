#!/usr/bin/env node
// Generates an offline visual review contact sheet for Legion UI icons and buttons.
// Not linked from the app. Writes HTML, PNGs, and centring measurements to:
//   ~/Documents/legion/ui-shots/contact-sheet/ (override with --out <dir>)
//
// Usage:
//   node scripts/ui-contact-sheet.mjs
//   node scripts/ui-contact-sheet.mjs --out /path/to/dir

import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, '..');
const UI_ROOT = path.join(REPO_ROOT, 'apps', 'ui');
const ICONS_PATH = path.join(UI_ROOT, 'icons.js');
const STYLES_PATH = path.join(UI_ROOT, 'styles.css');

const require = createRequire(import.meta.url);
const { chromium } = require(path.join(UI_ROOT, 'test', 'node_modules', 'playwright-core'));

function parseArgs(argv) {
  let outDir = path.join(os.homedir(), 'Documents', 'legion', 'ui-shots', 'contact-sheet');
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--out' && argv[i + 1]) {
      outDir = path.resolve(argv[i + 1]);
      i += 1;
    } else if (arg.startsWith('--out=')) {
      outDir = path.resolve(arg.slice('--out='.length));
    } else if (arg === '-h' || arg === '--help') {
      process.stdout.write(
        'Usage: node scripts/ui-contact-sheet.mjs [--out <dir>]\n' +
          'Default output: ~/Documents/legion/ui-shots/contact-sheet\n',
      );
      process.exit(0);
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }
  return { outDir };
}

function findBrowser() {
  if (process.env.LEGION_CHROME && fs.existsSync(process.env.LEGION_CHROME)) {
    return process.env.LEGION_CHROME;
  }
  const candidates = [
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/usr/bin/google-chrome',
    '/usr/bin/google-chrome-stable',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error('No Chromium or Chrome binary found; set LEGION_CHROME.');
}

function parseIconSymbols(source) {
  const symbols = [];
  const re = /<symbol\s+id="i-([^"]+)"\s+viewBox="([^"]+)">([\s\S]*?)<\/symbol>/g;
  let match;
  while ((match = re.exec(source)) !== null) {
    symbols.push({
      name: match[1],
      id: `i-${match[1]}`,
      viewBox: match[2],
      inner: match[3].trim(),
    });
  }
  if (symbols.length === 0) {
    throw new Error(`No <symbol id="i-..."> entries found in ${ICONS_PATH}`);
  }
  return symbols;
}

function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

function buildSpriteMarkup(symbols) {
  const items = symbols
    .map((s) => `<symbol id="${escapeHtml(s.id)}" viewBox="${escapeHtml(s.viewBox)}">${s.inner}</symbol>`)
    .join('\n');
  return `<svg class="sprite" aria-hidden="true">${items}</svg>`;
}

function iconMarkup(name) {
  return `<svg aria-hidden="true"><use href="#i-${escapeHtml(name)}" /></svg>`;
}

function buildIconsPageHtml(symbols, theme = 'dark') {
  const cards = symbols
    .map(
      (s) => `
      <div class="cs-icon-card" data-icon="${escapeHtml(s.name)}">
        <div class="cs-target-wrap">
          <button type="button" class="btn icon cs-icon-target" aria-label="${escapeHtml(s.name)}">
            ${iconMarkup(s.name)}
          </button>
          <span class="cs-guide-box" aria-hidden="true"></span>
          <span class="cs-guide-cross-h" aria-hidden="true"></span>
          <span class="cs-guide-cross-v" aria-hidden="true"></span>
        </div>
        <div class="cs-icon-name">${escapeHtml(s.name)}</div>
        <div class="cs-icon-meta" data-meta-for="${escapeHtml(s.name)}"></div>
      </div>`,
    )
    .join('\n');

  return `<!doctype html>
<html lang="en" data-theme="${escapeHtml(theme)}">
<head>
  <meta charset="utf-8" />
  <title>Legion UI Contact Sheet — Icons (${escapeHtml(theme)})</title>
  <link rel="stylesheet" href="/styles.css" />
  <style>
    html, body { height: auto; min-height: 100%; }
    body { padding: 24px; background: var(--bg); color: var(--text); }
    .cs-head { display: flex; justify-content: space-between; align-items: baseline; gap: 16px; margin-bottom: 18px; }
    .cs-head h1 { margin: 0; font: 650 20px/1.2 var(--sans); letter-spacing: -0.015em; }
    .cs-head p { margin: 0; font: 12px/1.4 var(--mono); color: var(--dim); }
    .cs-icon-grid {
      display: grid;
      grid-template-columns: repeat(6, minmax(0, 1fr));
      gap: 12px;
    }
    .cs-icon-card {
      display: grid;
      justify-items: center;
      gap: 8px;
      padding: 14px 10px 12px;
      border: 1px solid var(--line);
      border-radius: var(--radius-s);
      background: var(--surface);
    }
    .cs-target-wrap {
      position: relative;
      display: inline-grid;
      place-items: center;
    }
    /* Real .btn.icon hit target with icon explicitly drawn at 24px as specified. */
    .btn.icon.cs-icon-target {
      position: relative;
      z-index: 1;
      color: var(--text);
    }
    .btn.icon.cs-icon-target svg {
      width: 24px;
      height: 24px;
    }
    .cs-guide-box {
      position: absolute;
      inset: 0;
      border: 1px dashed color-mix(in srgb, var(--accent) 70%, transparent);
      border-radius: var(--radius-s);
      pointer-events: none;
      z-index: 2;
    }
    .cs-guide-cross-h {
      position: absolute;
      left: -6px;
      right: -6px;
      top: 50%;
      height: 1px;
      background: color-mix(in srgb, var(--bad) 55%, transparent);
      pointer-events: none;
      z-index: 2;
    }
    .cs-guide-cross-v {
      position: absolute;
      top: -6px;
      bottom: -6px;
      left: 50%;
      width: 1px;
      background: color-mix(in srgb, var(--bad) 55%, transparent);
      pointer-events: none;
      z-index: 2;
    }
    .cs-icon-name {
      font: 600 12px/1.2 var(--mono);
      color: var(--text);
      text-align: center;
      word-break: break-word;
    }
    .cs-icon-meta {
      font: 11px/1.3 var(--mono);
      color: var(--dim);
      text-align: center;
      white-space: pre;
    }
  </style>
</head>
<body>
  ${buildSpriteMarkup(symbols)}
  <header class="cs-head">
    <h1>Legion Icons — 24 px inside real <code>.btn.icon</code> hit target</h1>
    <p id="cs-dpr-label"></p>
  </header>
  <main class="cs-icon-grid">
    ${cards}
  </main>
</body>
</html>`;
}

const BUTTON_SPECS = [
  {
    id: 'btn-default',
    label: '.btn',
    note: 'Secondary outline button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="btn"${state === 'disabled' ? ' disabled' : ''}>Continue</button>`,
  },
  {
    id: 'btn-primary',
    label: '.btn.primary',
    note: 'Primary action button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="btn primary"${state === 'disabled' ? ' disabled' : ''}>Save changes</button>`,
  },
  {
    id: 'btn-danger',
    label: '.btn.danger',
    note: 'Destructive action button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="btn danger"${state === 'disabled' ? ' disabled' : ''}>Delete</button>`,
  },
  {
    id: 'btn-quiet',
    label: '.btn.quiet',
    note: 'Borderless subtle button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="btn quiet"${state === 'disabled' ? ' disabled' : ''}>Not now</button>`,
  },
  {
    id: 'btn-small',
    label: '.btn.small',
    note: 'Compact inline button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="btn small"${state === 'disabled' ? ' disabled' : ''}>Retry</button>`,
  },
  {
    id: 'btn-icon',
    label: '.btn.icon',
    note: 'Icon-only hit target (46x46)',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="btn icon" aria-label="More options"${state === 'disabled' ? ' disabled' : ''}>${iconMarkup('more')}</button>`,
  },
  {
    id: 'btn-icon-back',
    label: '.top .btn.icon.back',
    note: 'Header back button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="cs-top-wrap"><button type="button" class="btn icon back" aria-label="Back"${state === 'disabled' ? ' disabled' : ''}>${iconMarkup('back')}</button></div>`,
  },
  {
    id: 'btn-player-seek',
    label: '.btn.icon.player-center-btn',
    note: 'Video ±10s centre button (player: absent when N/A)',
    mediaBackdrop: true,
    absentWhenUnavailable: true,
    render: () =>
      `<button type="button" class="btn icon player-center-btn" aria-label="Back 10 seconds">${iconMarkup('skip-back-10')}</button>`,
  },
  {
    id: 'btn-player-play',
    label: '.btn.icon.player-center-btn.main',
    note: 'Video main play/pause button (player: absent when N/A)',
    mediaBackdrop: true,
    absentWhenUnavailable: true,
    render: () =>
      `<button type="button" class="btn icon player-center-btn main" aria-label="Play">${iconMarkup('play')}</button>`,
  },
  {
    id: 'btn-player-speed',
    label: '.btn.player-speed-btn',
    note: 'Video speed selector (player: absent when N/A)',
    mediaBackdrop: true,
    absentWhenUnavailable: true,
    render: () => `<button type="button" class="btn player-speed-btn">1.5×</button>`,
  },
  {
    id: 'btn-music-ctl',
    label: '.btn.icon.music-ctl-btn',
    note: 'Music prev/next button (player: absent when N/A)',
    mediaBackdrop: true,
    absentWhenUnavailable: true,
    render: () =>
      `<button type="button" class="btn icon music-ctl-btn" aria-label="Next">${iconMarkup('next')}</button>`,
  },
  {
    id: 'btn-music-play',
    label: '.btn.icon.music-play-btn',
    note: 'Music main play/pause button (player: absent when N/A)',
    mediaBackdrop: true,
    absentWhenUnavailable: true,
    render: () =>
      `<button type="button" class="btn icon music-play-btn" aria-label="Play">${iconMarkup('play')}</button>`,
  },
  {
    id: 'btn-player-resume',
    label: '.player-resume-btn',
    note: 'Start over inside resume chip (absent after 5 s)',
    mediaBackdrop: true,
    absentWhenUnavailable: true,
    render: () =>
      `<span class="player-resume-chip inline"><span>Resumed at 12:40</span><button type="button" class="player-resume-btn">Start over</button></span>`,
  },
  {
    id: 'btn-chip',
    label: '.chip-btn',
    note: 'Toggle chip (unpressed / aria-pressed)',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="cs-pair">
        <button type="button" class="chip-btn" aria-pressed="false"${state === 'disabled' ? ' disabled' : ''}>1×</button>
        <button type="button" class="chip-btn" aria-pressed="true"${state === 'disabled' ? ' disabled' : ''}>1.5×</button>
      </div>`,
  },
  {
    id: 'btn-seg',
    label: '.seg button',
    note: 'Segmented control (unpressed / aria-pressed)',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="seg cs-seg">
        <button type="button" aria-pressed="false"${state === 'disabled' ? ' disabled' : ''}>List</button>
        <button type="button" aria-pressed="true"${state === 'disabled' ? ' disabled' : ''}>Grid</button>
      </div>`,
  },
  {
    id: 'btn-tabs',
    label: '.tabs button',
    note: 'Machine detail tabs (unselected / aria-selected)',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="tabs cs-tabs" role="tablist">
        <button type="button" role="tab" aria-selected="false"${state === 'disabled' ? ' disabled' : ''}>Overview</button>
        <button type="button" role="tab" aria-selected="true"${state === 'disabled' ? ' disabled' : ''}>Files</button>
      </div>`,
  },
  {
    id: 'btn-filters',
    label: '.filters button',
    note: 'Files kind filter pills',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="filters cs-filters">
        <button type="button" aria-pressed="false"${state === 'disabled' ? ' disabled' : ''}>Video</button>
        <button type="button" aria-pressed="true"${state === 'disabled' ? ' disabled' : ''}>Audio</button>
      </div>`,
  },
  {
    id: 'btn-bar4',
    label: '.bar4 button',
    note: '4-column action bar (default / .active)',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="bar4 cs-bar4">
        <button type="button"${state === 'disabled' ? ' disabled' : ''}>${iconMarkup('download')}Download</button>
        <button type="button" class="active"${state === 'disabled' ? ' disabled' : ''}>${iconMarkup('star')}Starred</button>
      </div>`,
  },
  {
    id: 'btn-viewer-bar4',
    label: '.viewer-bar .bar4 button',
    note: 'Viewer action bar on dark backdrop',
    mediaBackdrop: true,
    render: ({ state }) =>
      `<div class="viewer-bar cs-viewer-bar"><div class="bar4 cs-bar4">
        <button type="button"${state === 'disabled' ? ' disabled' : ''}>${iconMarkup('download')}Download</button>
        <button type="button" class="active"${state === 'disabled' ? ' disabled' : ''}>${iconMarkup('star')}Starred</button>
      </div></div>`,
  },
  {
    id: 'btn-swipe',
    label: '.swipe-action',
    note: 'Row swipe actions (danger / neutral / accent)',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<div class="cs-swipe-group">
        <button type="button" class="swipe-action"${state === 'disabled' ? ' disabled' : ''}>Delete</button>
        <button type="button" class="swipe-action neutral"${state === 'disabled' ? ' disabled' : ''}>Rename</button>
        <button type="button" class="swipe-action accent"${state === 'disabled' ? ' disabled' : ''}>Star</button>
      </div>`,
  },
  {
    id: 'btn-update-dismiss',
    label: '.update-note-dismiss',
    note: 'Quiet post-update dismiss button',
    mediaBackdrop: false,
    render: ({ state }) =>
      `<button type="button" class="update-note-dismiss" aria-label="Dismiss"${state === 'disabled' ? ' disabled' : ''}>×</button>`,
  },
];

function buildButtonsPageHtml(symbols, theme) {
  const rows = BUTTON_SPECS.map((spec) => {
    const backdropCls = spec.mediaBackdrop ? ' cs-cell-dark' : '';
    const disabledCell = spec.absentWhenUnavailable
      ? `<span class="cs-absent-slot">absent (no disabled state)</span>`
      : spec.render({ state: 'disabled' });
    return `
      <tr data-spec="${escapeHtml(spec.id)}">
        <th scope="row">
          <div class="cs-spec-label">${escapeHtml(spec.label)}</div>
          <div class="cs-spec-note">${escapeHtml(spec.note)}</div>
        </th>
        <td class="cs-cell${backdropCls}" data-state="resting">${spec.render({ state: 'resting' })}</td>
        <td class="cs-cell${backdropCls}" data-state="pressed">${spec.render({ state: 'pressed' })}</td>
        <td class="cs-cell${backdropCls}" data-state="focused">${spec.render({ state: 'focused' })}</td>
        <td class="cs-cell${backdropCls}" data-state="disabled">${disabledCell}</td>
      </tr>`;
  }).join('\n');

  return `<!doctype html>
<html lang="en" data-theme="${escapeHtml(theme)}">
<head>
  <meta charset="utf-8" />
  <title>Legion UI Contact Sheet — Buttons (${escapeHtml(theme)})</title>
  <link rel="stylesheet" href="/styles.css" />
  <style>
    html, body { height: auto; min-height: 100%; }
    body { padding: 24px; background: var(--bg); color: var(--text); }
    .cs-head { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 18px; }
    .cs-head h1 { margin: 0; font: 650 20px/1.2 var(--sans); letter-spacing: -0.015em; }
    .cs-head p { margin: 0; font: 12px/1.4 var(--mono); color: var(--dim); }
    .cs-table {
      width: 100%;
      border-collapse: separate;
      border-spacing: 0;
      border: 1px solid var(--line);
      border-radius: var(--radius);
      overflow: hidden;
      background: var(--surface);
    }
    .cs-table th, .cs-table td {
      border-bottom: 1px solid var(--line);
      border-right: 1px solid var(--line);
      padding: 12px 14px;
      vertical-align: middle;
    }
    .cs-table tr:last-child th, .cs-table tr:last-child td { border-bottom: 0; }
    .cs-table th:last-child, .cs-table td:last-child { border-right: 0; }
    .cs-table thead th {
      background: var(--raised);
      font: 600 11px/1.2 var(--mono);
      letter-spacing: 0.08em;
      text-transform: uppercase;
      color: var(--dim);
      text-align: left;
    }
    .cs-table tbody th {
      width: 240px;
      text-align: left;
      background: var(--surface);
    }
    .cs-spec-label { font: 600 12.5px/1.3 var(--mono); color: var(--text); }
    .cs-spec-note { font: 11.5px/1.35 var(--sans); color: var(--dim); margin-top: 2px; }
    .cs-cell { text-align: center; background: var(--bg); }
    .cs-cell-dark { background: #0d0e10; color: #eceef0; }
    .cs-pair { display: inline-flex; gap: 8px; align-items: center; }
    .cs-seg { width: 150px; margin: 0 auto; }
    .cs-tabs { margin: 0; padding: 0; justify-content: center; }
    .cs-filters { padding: 0; justify-content: center; }
    .cs-bar4 { width: 170px; margin: 0 auto; }
    .cs-viewer-bar { padding: 4px; background: none; }
    .cs-swipe-group { display: inline-flex; height: 44px; border-radius: 8px; overflow: hidden; }
    .cs-top-wrap { display: inline-flex; align-items: center; justify-content: center; }
    .cs-top-wrap .btn.icon.back { margin: 0; color: var(--dim); }
    .cs-absent-slot {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-height: 44px;
      padding: 4px 12px;
      border: 1px dashed var(--line-strong);
      border-radius: var(--radius-s);
      font: 11px/1.3 var(--mono);
      color: var(--dim);
    }
    .cs-cell-dark .cs-absent-slot {
      border-color: rgb(255 255 255 / 0.22);
      color: rgb(255 255 255 / 0.56);
    }
  </style>
</head>
<body>
  ${buildSpriteMarkup(symbols)}
  <header class="cs-head">
    <h1>Legion Buttons — ${escapeHtml(theme)} theme</h1>
    <p>Resting · Pressed (:active) · Focused (:focus-visible) · Disabled or absent</p>
  </header>
  <table class="cs-table">
    <thead>
      <tr>
        <th>Kind</th>
        <th>Resting</th>
        <th>Pressed (:active)</th>
        <th>Focused (:focus-visible)</th>
        <th>Disabled / absent</th>
      </tr>
    </thead>
    <tbody>
      ${rows}
    </tbody>
  </table>
</body>
</html>`;
}

function buildReviewIndexHtml({ measurements, outDir }) {
  const rows = measurements
    .map((m) => {
      const maxDist = Math.max(m.vector.dist, m.dpr1.dist, m.dpr2.dist, m.dpr3.dist);
      const cls = maxDist <= 0.5 ? 'ok' : maxDist <= 1.5 ? 'warn' : 'off';
      const fmt = (o) => `${o.dx >= 0 ? '+' : ''}${o.dx.toFixed(2)}, ${o.dy >= 0 ? '+' : ''}${o.dy.toFixed(2)} (${o.dist.toFixed(2)})`;
      const extra = m.digitsVsCircle
        ? `digits vs circle: dx=${m.digitsVsCircle.vector.dx.toFixed(2)}, dy=${m.digitsVsCircle.vector.dy.toFixed(2)} | @1x (${m.digitsVsCircle.dpr1.dx.toFixed(2)}, ${m.digitsVsCircle.dpr1.dy.toFixed(2)}) @2x (${m.digitsVsCircle.dpr2.dx.toFixed(2)}, ${m.digitsVsCircle.dpr2.dy.toFixed(2)}) @3x (${m.digitsVsCircle.dpr3.dx.toFixed(2)}, ${m.digitsVsCircle.dpr3.dy.toFixed(2)})`
        : '';
      return `
        <tr class="sev-${cls}">
          <td><code>${escapeHtml(m.name)}</code></td>
          <td>${m.targetW.toFixed(0)}×${m.targetH.toFixed(0)}</td>
          <td><code>${fmt(m.vector)}</code></td>
          <td><code>${fmt(m.dpr1)}</code></td>
          <td><code>${fmt(m.dpr2)}</code></td>
          <td><code>${fmt(m.dpr3)}</code></td>
          <td>${escapeHtml(extra)}</td>
        </tr>`;
    })
    .join('\n');

  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Legion UI Contact Sheet</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #0d0e10;
      --surface: #15171a;
      --raised: #1d2024;
      --line: #262a2f;
      --text: #eceef0;
      --dim: #9da4ad;
      --accent: #b6f24a;
      --warn: #f2b84a;
      --bad: #ff6b5e;
      --sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
      --mono: ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace;
    }
    * { box-sizing: border-box; }
    body { margin: 0; padding: 28px; background: var(--bg); color: var(--text); font: 14px/1.5 var(--sans); }
    h1 { margin: 0 0 6px; font: 650 24px/1.2 var(--sans); letter-spacing: -0.02em; }
    h2 { margin: 28px 0 10px; font: 600 17px/1.25 var(--sans); }
    p.meta { margin: 0 0 20px; color: var(--dim); font: 12.5px/1.4 var(--mono); }
    .shots { display: grid; gap: 18px; }
    figure { margin: 0; padding: 14px; border: 1px solid var(--line); border-radius: 12px; background: var(--surface); }
    figcaption { margin: 0 0 10px; font: 600 13px/1.3 var(--mono); color: var(--dim); }
    figure img { display: block; max-width: 100%; height: auto; border-radius: 8px; border: 1px solid var(--line); }
    table { width: 100%; border-collapse: collapse; border: 1px solid var(--line); background: var(--surface); font-size: 12.5px; }
    th, td { padding: 8px 10px; border-bottom: 1px solid var(--line); border-right: 1px solid var(--line); text-align: left; }
    th { background: var(--raised); font: 600 11px/1.2 var(--mono); text-transform: uppercase; letter-spacing: 0.08em; color: var(--dim); }
    code { font-family: var(--mono); }
    tr.sev-ok td:first-child code { color: var(--accent); }
    tr.sev-warn td:first-child code { color: var(--warn); }
    tr.sev-off td:first-child code { color: var(--bad); }
  </style>
</head>
<body>
  <h1>Legion UI Contact Sheet</h1>
  <p class="meta">Icons (${measurements.length} symbols from apps/ui/icons.js at 24 px inside .btn.icon) &amp; buttons (dark + light). Output: ${escapeHtml(outDir)}</p>

  <h2>Icon Centring Measurements (shape bbox centre minus target centre, CSS px)</h2>
  <table>
    <thead>
      <tr>
        <th>Icon</th>
        <th>Target</th>
        <th>Vector dx, dy (dist)</th>
        <th>DPR 1 dx, dy (dist)</th>
        <th>DPR 2 dx, dy (dist)</th>
        <th>DPR 3 dx, dy (dist)</th>
        <th>Notes</th>
      </tr>
    </thead>
    <tbody>
      ${rows}
    </tbody>
  </table>

  <h2>Icons at DPR 1, 2, and 3 (Dark) &amp; DPR 2 (Light)</h2>
  <div class="shots">
    <figure><figcaption>icons-dpr1.png (deviceScaleFactor = 1)</figcaption><img src="./icons-dpr1.png" alt="Icons at DPR 1" /></figure>
    <figure><figcaption>icons-dpr2.png (deviceScaleFactor = 2)</figcaption><img src="./icons-dpr2.png" alt="Icons at DPR 2" /></figure>
    <figure><figcaption>icons-dpr3.png (deviceScaleFactor = 3)</figcaption><img src="./icons-dpr3.png" alt="Icons at DPR 3" /></figure>
    <figure><figcaption>icons-light-dpr2.png (light theme, deviceScaleFactor = 2)</figcaption><img src="./icons-light-dpr2.png" alt="Icons in light theme at DPR 2" /></figure>
  </div>

  <h2>Button Matrix (Dark &amp; Light)</h2>
  <div class="shots">
    <figure><figcaption>buttons-dark.png (dark theme, deviceScaleFactor = 2)</figcaption><img src="./buttons-dark.png" alt="Buttons in dark theme" /></figure>
    <figure><figcaption>buttons-light.png (light theme, deviceScaleFactor = 2)</figcaption><img src="./buttons-light.png" alt="Buttons in light theme" /></figure>
  </div>
</body>
</html>`;
}

async function startServer(pages) {
  const stylesBuf = fs.readFileSync(STYLES_PATH);
  const server = http.createServer((req, res) => {
    const pathname = new URL(req.url || '/', 'http://127.0.0.1').pathname;
    if (pathname === '/styles.css') {
      res.writeHead(200, { 'Content-Type': 'text/css; charset=utf-8' });
      res.end(stylesBuf);
      return;
    }
    if (Object.prototype.hasOwnProperty.call(pages, pathname)) {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(pages[pathname]);
      return;
    }
    res.writeHead(404);
    res.end('Not found');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const addr = server.address();
  return {
    url: `http://127.0.0.1:${addr.port}`,
    stop: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function measureIconsOnPage(page, dpr) {
  return page.evaluate(async (scaleFactor) => {
    const label = document.getElementById('cs-dpr-label');
    if (label) {
      label.textContent = `DPR ${scaleFactor} · target outline (dashed) · target centre cross (red)`;
    }

    async function rasterInkCenter(svgMarkup, cssW, cssH, dprVal) {
      const pxW = Math.round(cssW * dprVal);
      const pxH = Math.round(cssH * dprVal);
      const img = new Image();
      img.width = pxW;
      img.height = pxH;
      const url = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svgMarkup)}`;
      await new Promise((resolve, reject) => {
        img.onload = resolve;
        img.onerror = reject;
        img.src = url;
      });
      const canvas = document.createElement('canvas');
      canvas.width = pxW;
      canvas.height = pxH;
      const ctx = canvas.getContext('2d');
      ctx.drawImage(img, 0, 0, pxW, pxH);
      const data = ctx.getImageData(0, 0, pxW, pxH).data;
      let minX = pxW;
      let minY = pxH;
      let maxX = -1;
      let maxY = -1;
      for (let y = 0; y < pxH; y += 1) {
        for (let x = 0; x < pxW; x += 1) {
          if (data[(y * pxW + x) * 4 + 3] > 8) {
            if (x < minX) minX = x;
            if (x > maxX) maxX = x;
            if (y < minY) minY = y;
            if (y > maxY) maxY = y;
          }
        }
      }
      if (maxX < minX || maxY < minY) {
        return { cx: cssW / 2, cy: cssH / 2 };
      }
      return {
        cx: (minX + maxX + 1) / 2 / dprVal,
        cy: (minY + maxY + 1) / 2 / dprVal,
      };
    }

    const cards = Array.from(document.querySelectorAll('.cs-icon-card'));
    const results = [];

    for (const card of cards) {
      const name = card.getAttribute('data-icon');
      const btn = card.querySelector('.cs-icon-target');
      const svg = btn.querySelector('svg');
      const btnRect = btn.getBoundingClientRect();
      const svgRect = svg.getBoundingClientRect();
      const style = getComputedStyle(svg);
      const sym = document.getElementById(`i-${name}`);
      const viewBox = sym.getAttribute('viewBox') || '0 0 24 24';
      const [, , vbW, vbH] = viewBox.split(/\s+/).map(Number);

      // Exact SVG geometry bounding box inside the live 24x24 viewport.
      const probe = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
      probe.setAttribute('viewBox', viewBox);
      probe.setAttribute('width', String(svgRect.width));
      probe.setAttribute('height', String(svgRect.height));
      probe.style.position = 'fixed';
      probe.style.left = '-9999px';
      probe.innerHTML = sym.innerHTML;
      document.body.appendChild(probe);
      const allBox = probe.getBBox();
      const circleEl = probe.querySelector('[data-part="circle"]');
      const digitsEl = probe.querySelector('[data-part="digits"]');
      const circleBox = circleEl ? circleEl.getBBox() : null;
      const digitsBox = digitsEl ? digitsEl.getBBox() : null;
      probe.remove();

      const targetCx = btnRect.width / 2;
      const targetCy = btnRect.height / 2;
      const svgLeftInBtn = svgRect.left - btnRect.left;
      const svgTopInBtn = svgRect.top - btnRect.top;
      const sx = svgRect.width / vbW;
      const sy = svgRect.height / vbH;

      const vecCxInBtn = svgLeftInBtn + (allBox.x + allBox.width / 2) * sx;
      const vecCyInBtn = svgTopInBtn + (allBox.y + allBox.height / 2) * sy;
      const vecDx = vecCxInBtn - targetCx;
      const vecDy = vecCyInBtn - targetCy;

      const baseAttrs =
        `xmlns="http://www.w3.org/2000/svg" viewBox="${viewBox}" ` +
        `width="${svgRect.width}" height="${svgRect.height}" fill="none" stroke="#ffffff" ` +
        `stroke-width="${style.strokeWidth}" stroke-linecap="${style.strokeLinecap}" stroke-linejoin="${style.strokeLinejoin}"`;
      const fullInk = await rasterInkCenter(`<svg ${baseAttrs}>${sym.innerHTML}</svg>`, svgRect.width, svgRect.height, scaleFactor);
      const pxDx = svgLeftInBtn + fullInk.cx - targetCx;
      const pxDy = svgTopInBtn + fullInk.cy - targetCy;

      let digitsVsCircle = null;
      if (circleBox && digitsBox) {
        const vecDvcDx = (digitsBox.x + digitsBox.width / 2 - (circleBox.x + circleBox.width / 2)) * sx;
        const vecDvcDy = (digitsBox.y + digitsBox.height / 2 - (circleBox.y + circleBox.height / 2)) * sy;
        const circleInk = await rasterInkCenter(
          `<svg ${baseAttrs}>${sym.querySelector('[data-part="circle"]').outerHTML}</svg>`,
          svgRect.width,
          svgRect.height,
          scaleFactor,
        );
        const digitsInk = await rasterInkCenter(
          `<svg ${baseAttrs}>${sym.querySelector('[data-part="digits"]').outerHTML}</svg>`,
          svgRect.width,
          svgRect.height,
          scaleFactor,
        );
        digitsVsCircle = {
          vector: { dx: vecDvcDx, dy: vecDvcDy },
          pixel: { dx: digitsInk.cx - circleInk.cx, dy: digitsInk.cy - circleInk.cy },
        };
      }

      const metaEl = card.querySelector(`[data-meta-for="${name}"]`);
      if (metaEl) {
        metaEl.textContent =
          `vec ${vecDx >= 0 ? '+' : ''}${vecDx.toFixed(2)}, ${vecDy >= 0 ? '+' : ''}${vecDy.toFixed(2)}\n` +
          `px  ${pxDx >= 0 ? '+' : ''}${pxDx.toFixed(2)}, ${pxDy >= 0 ? '+' : ''}${pxDy.toFixed(2)}`;
      }

      results.push({
        name,
        targetW: btnRect.width,
        targetH: btnRect.height,
        iconW: svgRect.width,
        iconH: svgRect.height,
        vector: { dx: vecDx, dy: vecDy, dist: Math.hypot(vecDx, vecDy) },
        pixel: { dx: pxDx, dy: pxDy, dist: Math.hypot(pxDx, pxDy) },
        digitsVsCircle,
      });
    }

    return results;
  }, dpr);
}

async function applyForcedButtonPseudoStates(context, page) {
  const cdp = await context.newCDPSession(page);
  await cdp.send('DOM.enable');
  await cdp.send('CSS.enable');
  const { root } = await cdp.send('DOM.getDocument', { depth: -1 });

  const pressedNodes = await cdp.send('DOM.querySelectorAll', {
    nodeId: root.nodeId,
    selector: 'td[data-state="pressed"] button',
  });
  for (const nodeId of pressedNodes.nodeIds) {
    await cdp.send('CSS.forcePseudoState', {
      nodeId,
      forcedPseudoClasses: ['active'],
    });
  }

  const focusedNodes = await cdp.send('DOM.querySelectorAll', {
    nodeId: root.nodeId,
    selector: 'td[data-state="focused"] button',
  });
  for (const nodeId of focusedNodes.nodeIds) {
    await cdp.send('CSS.forcePseudoState', {
      nodeId,
      forcedPseudoClasses: ['focus', 'focus-visible'],
    });
  }

  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  return cdp;
}

async function main() {
  const { outDir } = parseArgs(process.argv.slice(2));
  fs.mkdirSync(outDir, { recursive: true });

  const iconSource = fs.readFileSync(ICONS_PATH, 'utf8');
  const symbols = parseIconSymbols(iconSource);

  const pages = {
    '/icons-dark.html': buildIconsPageHtml(symbols, 'dark'),
    '/icons-light.html': buildIconsPageHtml(symbols, 'light'),
    '/buttons-dark.html': buildButtonsPageHtml(symbols, 'dark'),
    '/buttons-light.html': buildButtonsPageHtml(symbols, 'light'),
  };

  const server = await startServer(pages);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  try {
    const byIcon = new Map();

    // 1. Icons at DPR 1, 2, and 3 (dark theme) + light theme at DPR 2.
    for (const dpr of [1, 2, 3]) {
      const context = await browser.newContext({
        viewport: { width: 1120, height: 900 },
        deviceScaleFactor: dpr,
        colorScheme: 'dark',
      });
      const page = await context.newPage();
      await page.goto(`${server.url}/icons-dark.html`, { waitUntil: 'load' });
      const rows = await measureIconsOnPage(page, dpr);
      await page.screenshot({
        path: path.join(outDir, `icons-dpr${dpr}.png`),
        fullPage: true,
      });
      for (const r of rows) {
        if (!byIcon.has(r.name)) {
          byIcon.set(r.name, {
            name: r.name,
            targetW: r.targetW,
            targetH: r.targetH,
            iconW: r.iconW,
            iconH: r.iconH,
            vector: r.vector,
            dpr1: null,
            dpr2: null,
            dpr3: null,
            digitsVsCircle: r.digitsVsCircle
              ? { vector: r.digitsVsCircle.vector, dpr1: null, dpr2: null, dpr3: null }
              : null,
          });
        }
        const entry = byIcon.get(r.name);
        entry[`dpr${dpr}`] = r.pixel;
        if (r.digitsVsCircle && entry.digitsVsCircle) {
          entry.digitsVsCircle[`dpr${dpr}`] = r.digitsVsCircle.pixel;
        }
      }
      await context.close();
    }

    {
      const context = await browser.newContext({
        viewport: { width: 1120, height: 900 },
        deviceScaleFactor: 2,
        colorScheme: 'light',
      });
      const page = await context.newPage();
      await page.goto(`${server.url}/icons-light.html`, { waitUntil: 'load' });
      await measureIconsOnPage(page, 2);
      await page.screenshot({
        path: path.join(outDir, 'icons-light-dpr2.png'),
        fullPage: true,
      });
      await context.close();
    }

    // 2. Button matrix in dark and light themes with real :active and :focus-visible states.
    for (const theme of ['dark', 'light']) {
      const context = await browser.newContext({
        viewport: { width: 1320, height: 1000 },
        deviceScaleFactor: 2,
        colorScheme: theme,
      });
      const page = await context.newPage();
      await page.goto(`${server.url}/buttons-${theme}.html`, { waitUntil: 'load' });
      const cdp = await applyForcedButtonPseudoStates(context, page);
      await page.screenshot({
        path: path.join(outDir, `buttons-${theme}.png`),
        fullPage: true,
      });
      await cdp.detach();
      await context.close();
    }

    const measurements = Array.from(byIcon.values());
    fs.writeFileSync(
      path.join(outDir, 'measurements.json'),
      `${JSON.stringify({ outDir, iconCount: measurements.length, measurements }, null, 2)}\n`,
    );
    fs.writeFileSync(path.join(outDir, 'index.html'), buildReviewIndexHtml({ measurements, outDir }));

    // Print aligned summary table.
    const pad = (s, n) => String(s).padEnd(n);
    const fmtPair = (o) => `${o.dx >= 0 ? '+' : ''}${o.dx.toFixed(2)},${o.dy >= 0 ? '+' : ''}${o.dy.toFixed(2)} (${o.dist.toFixed(2)})`;
    process.stdout.write(`Contact sheet written to: ${outDir}\n`);
    process.stdout.write(
      `${pad('icon', 18)} ${pad('vector dx,dy (d)', 22)} ${pad('dpr1 dx,dy (d)', 22)} ${pad('dpr2 dx,dy (d)', 22)} ${pad('dpr3 dx,dy (d)', 22)}\n`,
    );
    process.stdout.write(`${'-'.repeat(108)}\n`);
    for (const m of measurements) {
      process.stdout.write(
        `${pad(m.name, 18)} ${pad(fmtPair(m.vector), 22)} ${pad(fmtPair(m.dpr1), 22)} ${pad(fmtPair(m.dpr2), 22)} ${pad(fmtPair(m.dpr3), 22)}\n`,
      );
      if (m.digitsVsCircle) {
        const d = m.digitsVsCircle;
        process.stdout.write(
          `  └─ 10 vs circle: vec=(${d.vector.dx.toFixed(2)}, ${d.vector.dy.toFixed(2)}) ` +
            `@1x=(${d.dpr1.dx.toFixed(2)}, ${d.dpr1.dy.toFixed(2)}) ` +
            `@2x=(${d.dpr2.dx.toFixed(2)}, ${d.dpr2.dy.toFixed(2)}) ` +
            `@3x=(${d.dpr3.dx.toFixed(2)}, ${d.dpr3.dy.toFixed(2)})\n`,
        );
      }
    }
  } finally {
    await browser.close();
    await server.stop();
  }
}

main().catch((err) => {
  process.stderr.write(`${err?.stack || err}\n`);
  process.exit(1);
});
