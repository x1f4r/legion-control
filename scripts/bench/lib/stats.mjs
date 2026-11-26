// Statistical helpers for repeatable benchmark series (median, IQR, min/max, A/B deltas).

export function sorted(values) {
  return [...values].filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
}

export function quantile(sortedValues, p) {
  if (sortedValues.length === 0) return null;
  if (sortedValues.length === 1) return sortedValues[0];
  const idx = (sortedValues.length - 1) * p;
  const lo = Math.floor(idx);
  const hi = Math.ceil(idx);
  if (lo === hi) return sortedValues[lo];
  return sortedValues[lo] + (sortedValues[hi] - sortedValues[lo]) * (idx - lo);
}

export function summarize(values) {
  const s = sorted(values);
  if (s.length === 0) {
    return { n: 0, median: null, p25: null, p75: null, iqr: null, min: null, max: null, samples: [] };
  }
  const median = quantile(s, 0.5);
  const p25 = quantile(s, 0.25);
  const p75 = quantile(s, 0.75);
  return {
    n: s.length,
    median,
    p25,
    p75,
    iqr: p75 - p25,
    min: s[0],
    max: s[s.length - 1],
    samples: values,
  };
}

/**
 * Summarizes an array of sample objects `{ metricA: number, metricB: number, ... }`
 * into `{ metricA: summary, metricB: summary, ... }`.
 */
export function summarizeSamples(samples) {
  const keys = new Set();
  for (const s of samples) {
    if (!s || typeof s !== 'object') continue;
    for (const [k, v] of Object.entries(s)) {
      if (typeof v === 'number' && Number.isFinite(v)) keys.add(k);
    }
  }
  const out = {};
  for (const k of keys) {
    out[k] = summarize(samples.map((s) => s?.[k]).filter((v) => typeof v === 'number' && Number.isFinite(v)));
  }
  return out;
}

export function formatNum(v, decimals = 1) {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—';
  if (Math.abs(v) >= 1000 && decimals === 1) return Math.round(v).toLocaleString('en-US');
  return v.toFixed(decimals);
}

export function formatSpread(stat, unit = '', decimals = 1) {
  if (!stat || stat.median === null) return '—';
  const med = formatNum(stat.median, decimals);
  const u = unit ? ` ${unit}` : '';
  if (stat.n <= 1) return `${med}${u}`;
  const lo = formatNum(stat.min, decimals);
  const hi = formatNum(stat.max, decimals);
  const iqr = formatNum(stat.iqr, decimals);
  return `${med}${u} (IQR ${iqr}, ${lo}–${hi})`;
}

/**
 * Computes relative change and speedup between baseline and candidate medians.
 * `higherIsBetter`: true for throughput (MiB/s), false for latency/CPU/RSS/dropped frames.
 */
export function compareStats(baseStat, candStat, higherIsBetter = false) {
  if (!baseStat || !candStat || baseStat.median === null || candStat.median === null) {
    return null;
  }
  const b = baseStat.median;
  const c = candStat.median;
  if (b === 0) {
    return {
      pctDelta: c === 0 ? 0 : null,
      speedup: c === 0 ? 1 : null,
      improved: c === 0 ? null : (higherIsBetter ? c > 0 : c < 0),
    };
  }
  const pctDelta = ((c - b) / b) * 100;
  const speedup = higherIsBetter ? c / b : b / Math.max(c, 1e-9);
  const improved = higherIsBetter ? c > b * 1.02 : c < b * 0.98;
  const regressed = higherIsBetter ? c < b * 0.98 : c > b * 1.02;
  return { pctDelta, speedup, improved, regressed };
}
