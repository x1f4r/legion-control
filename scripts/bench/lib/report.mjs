// Generates a self-contained, plain, readable HTML performance report (no external dependencies).

import { compareStats, formatNum, formatSpread } from './stats.mjs';

function esc(str) {
  return String(str ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

function renderDelta(baseStat, candStat, higherIsBetter) {
  const cmp = compareStats(baseStat, candStat, higherIsBetter);
  if (!cmp || cmp.pctDelta === null) return '<span class="delta flat">—</span>';
  const sign = cmp.pctDelta >= 0 ? '+' : '';
  const cls = cmp.improved ? 'good' : cmp.regressed ? 'bad' : 'flat';
  const ratio =
    cmp.speedup !== null && Number.isFinite(cmp.speedup) && Math.abs(cmp.pctDelta) >= 15
      ? ` (${cmp.speedup.toFixed(2)}×)`
      : '';
  return `<span class="delta ${cls}">${sign}${cmp.pctDelta.toFixed(1)}%${ratio}</span>`;
}

function renderCell(stat, unit, decimals) {
  if (!stat || stat.median === null) return '<td class="num muted">—</td>';
  const main = `${formatNum(stat.median, decimals)}${unit ? ` ${unit}` : ''}`;
  const sub =
    stat.n > 1
      ? `<div class="sub">IQR ${formatNum(stat.iqr, decimals)} · ${formatNum(stat.min, decimals)}–${formatNum(stat.max, decimals)}</div>`
      : '';
  return `<td class="num"><div class="val">${esc(main)}</div>${sub}</td>`;
}

function renderSuiteSection(suite, hasCompare, labels) {
  const rowsHtml = suite.rows
    .map((row) => {
      const cols = suite.columns
        .map((col) => {
          const candStat = row.candidate?.[col.key];
          if (!hasCompare) {
            return renderCell(candStat, col.unit, col.decimals ?? 1);
          }
          const baseStat = row.baseline?.[col.key];
          const deltaHtml = col.primary
            ? renderDelta(baseStat, candStat, Boolean(col.higherIsBetter))
            : '';
          if (!col.primary) {
            const bStr = baseStat?.median != null ? formatNum(baseStat.median, col.decimals ?? 1) : '—';
            const cStr = candStat?.median != null ? formatNum(candStat.median, col.decimals ?? 1) : '—';
            const u = col.unit ? ` ${col.unit}` : '';
            return `<td class="num"><div class="val">${esc(cStr)}${esc(u)}</div><div class="sub">was ${esc(bStr)}${esc(u)}</div></td>`;
          }
          return [
            renderCell(baseStat, col.unit, col.decimals ?? 1),
            renderCell(candStat, col.unit, col.decimals ?? 1),
            `<td class="num">${deltaHtml}</td>`,
          ].join('');
        })
        .join('');
      const note = row.note ? `<div class="row-note">${esc(row.note)}</div>` : '';
      return `<tr><td class="label"><strong>${esc(row.name)}</strong>${note}</td>${cols}</tr>`;
    })
    .join('\n');

  const headersHtml = suite.columns
    .map((col) => {
      if (!hasCompare || !col.primary) {
        return `<th class="num">${esc(col.label)}</th>`;
      }
      return [
        `<th class="num">${esc(col.label)}<div class="th-sub">${esc(labels.baseline)}</div></th>`,
        `<th class="num">${esc(col.label)}<div class="th-sub">${esc(labels.candidate)}</div></th>`,
        `<th class="num">Delta</th>`,
      ].join('');
    })
    .join('');

  const loadLine = suite.loadBefore
    ? `<div class="load-meta">Host load before series: <code>${esc(suite.loadBefore.raw)}</code></div>`
    : '';
  const notesHtml =
    suite.notes && suite.notes.length > 0
      ? `<ul class="suite-notes">${suite.notes.map((n) => `<li>${esc(n)}</li>`).join('')}</ul>`
      : '';

  return `
  <section class="suite">
    <div class="suite-head">
      <h2>${esc(suite.title)}</h2>
      ${loadLine}
    </div>
    ${suite.description ? `<p class="desc">${esc(suite.description)}</p>` : ''}
    <div class="table-wrap">
      <table>
        <thead>
          <tr><th>Benchmark</th>${headersHtml}</tr>
        </thead>
        <tbody>
          ${rowsHtml}
        </tbody>
      </table>
    </div>
    ${notesHtml}
  </section>`;
}

export function renderHtmlReport(report) {
  const m = report.machine;
  const hasCompare = Boolean(report.compare);
  const labels = {
    baseline: report.compare?.label || 'baseline',
    candidate: report.candidate.label || 'candidate',
  };

  // Build headline cards for primary metrics across suites.
  const highlights = [];
  for (const suite of report.suites) {
    const primaryCol = suite.columns.find((c) => c.primary);
    if (!primaryCol) continue;
    for (const row of suite.rows) {
      if (!row.highlight) continue;
      const candStat = row.candidate?.[primaryCol.key];
      const baseStat = hasCompare ? row.baseline?.[primaryCol.key] : null;
      const cmp = hasCompare ? compareStats(baseStat, candStat, Boolean(primaryCol.higherIsBetter)) : null;
      highlights.push({
        suite: suite.title,
        name: row.name,
        value: formatSpread(candStat, primaryCol.unit, primaryCol.decimals ?? 1),
        baseValue: baseStat ? formatSpread(baseStat, primaryCol.unit, primaryCol.decimals ?? 1) : null,
        deltaHtml: hasCompare ? renderDelta(baseStat, candStat, Boolean(primaryCol.higherIsBetter)) : '',
        improved: cmp?.improved,
      });
    }
  }

  const highlightsHtml = highlights.length
    ? `<section class="cards">${highlights
        .map(
          (h) => `
      <div class="card">
        <div class="card-kicker">${esc(h.suite)}</div>
        <div class="card-title">${esc(h.name)}</div>
        <div class="card-val">${esc(h.value)}</div>
        ${hasCompare ? `<div class="card-cmp">Baseline: ${esc(h.baseValue)} ${h.deltaHtml}</div>` : ''}
      </div>`,
        )
        .join('')}</section>`
    : '';

  const suitesHtml = report.suites.map((s) => renderSuiteSection(s, hasCompare, labels)).join('\n');

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Legion Performance Report — ${esc(m.gitRev)}</title>
<style>
  :root {
    --bg: #0f1115;
    --surface: #171a21;
    --surface-2: #1e232d;
    --border: #2a303d;
    --text: #e7ecf3;
    --muted: #98a2b3;
    --accent: #6aa9ff;
    --good: #34d399;
    --good-bg: rgba(52, 211, 153, 0.14);
    --bad: #f87171;
    --bad-bg: rgba(248, 113, 113, 0.14);
    --flat-bg: rgba(152, 162, 179, 0.14);
    --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    --sans: system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial, sans-serif;
  }
  @media (prefers-color-scheme: light) {
    :root {
      --bg: #f6f8fb;
      --surface: #ffffff;
      --surface-2: #f1f5f9;
      --border: #dce3ee;
      --text: #111827;
      --muted: #526071;
      --accent: #1d4ed8;
      --good: #047857;
      --good-bg: rgba(4, 120, 87, 0.1);
      --bad: #b91c1c;
      --bad-bg: rgba(185, 28, 28, 0.1);
      --flat-bg: rgba(82, 96, 113, 0.1);
    }
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    padding: 28px 24px 64px;
    background: var(--bg);
    color: var(--text);
    font-family: var(--sans);
    line-height: 1.5;
  }
  .wrap { max-width: 1280px; margin: 0 auto; }
  header {
    border-bottom: 1px solid var(--border);
    padding-bottom: 18px;
    margin-bottom: 24px;
  }
  h1 { margin: 0 0 8px; font-size: 1.7rem; letter-spacing: -0.02em; }
  h2 { margin: 0; font-size: 1.2rem; letter-spacing: -0.01em; }
  .meta-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: 10px 18px;
    margin-top: 14px;
    padding: 12px 14px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    font-size: 0.88rem;
  }
  .meta-grid dt { color: var(--muted); font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.04em; }
  .meta-grid dd { margin: 2px 0 0; font-family: var(--mono); word-break: break-word; }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 14px;
    margin-bottom: 28px;
  }
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 14px 16px;
  }
  .card-kicker { font-size: 0.75rem; color: var(--muted); text-transform: uppercase; letter-spacing: 0.05em; }
  .card-title { font-weight: 600; margin-top: 2px; font-size: 0.95rem; }
  .card-val { font-family: var(--mono); font-size: 1.08rem; font-weight: 600; margin-top: 6px; color: var(--accent); }
  .card-cmp { font-size: 0.82rem; color: var(--muted); margin-top: 6px; }
  .suite {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 18px 20px;
    margin-bottom: 22px;
  }
  .suite-head {
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    align-items: baseline;
    gap: 10px;
    margin-bottom: 8px;
  }
  .load-meta { font-size: 0.8rem; color: var(--muted); }
  .load-meta code { font-family: var(--mono); background: var(--surface-2); padding: 2px 6px; border-radius: 5px; }
  .desc { margin: 0 0 12px; color: var(--muted); font-size: 0.9rem; }
  .table-wrap { overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; font-size: 0.88rem; }
  th, td { padding: 9px 10px; border-bottom: 1px solid var(--border); vertical-align: top; }
  thead th {
    text-align: left;
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    background: var(--surface-2);
    white-space: nowrap;
  }
  th.num, td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
  .th-sub { font-weight: 500; text-transform: none; letter-spacing: 0; font-size: 0.74rem; color: var(--text); margin-top: 2px; }
  .val { font-family: var(--mono); font-weight: 600; }
  .sub { font-family: var(--mono); font-size: 0.75rem; color: var(--muted); margin-top: 2px; }
  .row-note { font-size: 0.78rem; color: var(--muted); margin-top: 2px; font-weight: 400; }
  .muted { color: var(--muted); }
  .delta {
    display: inline-block;
    font-family: var(--mono);
    font-size: 0.8rem;
    font-weight: 600;
    padding: 2px 7px;
    border-radius: 999px;
  }
  .delta.good { color: var(--good); background: var(--good-bg); }
  .delta.bad { color: var(--bad); background: var(--bad-bg); }
  .delta.flat { color: var(--muted); background: var(--flat-bg); }
  .suite-notes { margin: 12px 0 0; padding-left: 18px; color: var(--muted); font-size: 0.84rem; }
  footer { margin-top: 26px; font-size: 0.82rem; color: var(--muted); }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <h1>Legion Performance Pass${hasCompare ? ` — ${esc(labels.baseline)} vs ${esc(labels.candidate)}` : ` — ${esc(labels.candidate)}`}</h1>
    <div style="color:var(--muted);font-size:0.92rem;">
      Repeatable loopback measurements (median, IQR, min–max across repeated${hasCompare ? ' interleaved' : ''} runs).
    </div>
    <dl class="meta-grid">
      <div><dt>Revision</dt><dd>${esc(m.gitBranch)} @ ${esc(m.gitRev)}${m.gitDirty ? ' (dirty)' : ''}</dd></div>
      <div><dt>CPU</dt><dd>${esc(m.cpuModel)} (${esc(m.physicalCores ?? '?')}C / ${esc(m.logicalCpus)}T)</dd></div>
      <div><dt>Memory &amp; OS</dt><dd>${esc(m.totalRamGiB)} GiB · ${esc(m.os)} (${esc(m.kernel)})</dd></div>
      <div><dt>Candidate Binary</dt><dd>${esc(report.candidate.bin)}</dd></div>
      ${hasCompare ? `<div><dt>Baseline Binary</dt><dd>${esc(report.compare.bin)}</dd></div>` : ''}
      <div><dt>Generated</dt><dd>${esc(m.timestamp)}</dd></div>
    </dl>
  </header>

  ${highlightsHtml}
  ${suitesHtml}

  <footer>
    Raw machine-readable data saved alongside this file in <code>results.json</code>.
  </footer>
</div>
</body>
</html>
`;
}
