import assert from 'node:assert/strict';
import test from 'node:test';
import {
  clampPct,
  diskUsagePct,
  firstSentence,
  formatBytePair,
  formatBytes,
  formatDuration,
  formatDurationMs,
  formatElapsed,
  formatRelativeTime,
  machineMetrics,
  meterLevel,
  osLabel,
  primaryDisk,
  realDisks,
  splitFirstSentence,
  tokenizeCode,
} from '../format.js';

test('clampPct and meterLevel handle bounds and thresholds', () => {
  assert.equal(clampPct(-5), 0);
  assert.equal(clampPct(48.6), 49);
  assert.equal(clampPct(150), 100);
  assert.equal(clampPct(Number.NaN), 0);

  assert.equal(meterLevel(0), '');
  assert.equal(meterLevel(69), '');
  assert.equal(meterLevel(70), 'high');
  assert.equal(meterLevel(89), 'high');
  assert.equal(meterLevel(90), 'full');
  assert.equal(meterLevel(100), 'full');
});

test('formatBytes and formatBytePair scale cleanly', () => {
  assert.equal(formatBytes(0), '0 B');
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(1500), '1.5 KB');
  assert.equal(formatBytes(247_000_000_000), '247 GB');
  assert.equal(formatBytes(2_800_000_000_000), '2.8 TB');

  assert.equal(formatBytePair(29e9, 60e9), '29 of 60 GB');
  assert.equal(formatBytePair(247e9, 2.8e12), '247 GB of 2.8 TB');
  assert.equal(formatBytePair(11e9, 16e9), '11 of 16 GB');
});

test('duration, elapsed, and relative time formatting', () => {
  assert.equal(formatDuration(45), '45 s');
  assert.equal(formatDuration(180), '3 min');
  assert.equal(formatDuration(11 * 3600 + 26 * 60), '11 h 26 min');
  assert.equal(formatDuration(41 * 86400), '41 days');

  assert.equal(formatDurationMs(38), '38 ms');
  assert.equal(formatDurationMs(1420), '1.4 s');
  assert.equal(formatDurationMs(125_000), '2 min');

  assert.equal(formatElapsed(252), '4:12');
  assert.equal(formatElapsed(3820), '1:03:40');

  const now = Date.parse('2026-11-24T12:00:00Z');
  assert.equal(formatRelativeTime('2026-11-24T11:59:40Z', now), 'just now');
  assert.equal(formatRelativeTime('2026-11-24T11:56:00Z', now), '4 min ago');
  assert.equal(formatRelativeTime('2026-11-24T11:00:00Z', now), '1 h ago');
  assert.equal(formatRelativeTime('2026-11-10T12:00:00Z', now), '14 days ago');
});

test('firstSentence, splitFirstSentence, tokenizeCode, and osLabel', () => {
  assert.equal(
    firstSentence('Restart this machine.\nSecond sentence here.'),
    'Restart this machine.',
  );
  assert.deepEqual(splitFirstSentence('First sentence. Second one. Third.'), {
    first: 'First sentence.',
    rest: 'Second one. Third.',
  });
  assert.deepEqual(splitFirstSentence('Only one sentence.'), {
    first: 'Only one sentence.',
    rest: '',
  });
  assert.equal(firstSentence('Single line without period'), 'Single line without period');

  assert.deepEqual(tokenizeCode('Wait `delay_s` seconds, or check `would_refuse`.'), [
    { code: false, text: 'Wait ' },
    { code: true, text: 'delay_s' },
    { code: false, text: ' seconds, or check ' },
    { code: true, text: 'would_refuse' },
    { code: false, text: '.' },
  ]);
  assert.deepEqual(tokenizeCode('Plain text with `unclosed tick'), [
    { code: false, text: 'Plain text with `unclosed tick' },
  ]);

  assert.equal(osLabel('linux'), 'Linux');
  assert.equal(osLabel('macos'), 'macOS');
  assert.equal(osLabel('windows'), 'Windows');
});

test('primaryDisk ignores /efi when real disks exist and picks fullest', () => {
  const disks = [
    { mount: '/efi', fs: 'vfat', total: 2e9, available: 0.2e9 },
    { mount: '/', fs: 'btrfs', total: 2.8e12, available: 2.55e12 },
    { mount: '/data', fs: 'ext4', total: 4e12, available: 0.4e12 },
  ];
  assert.equal(realDisks(disks).length, 2);
  const full = primaryDisk(disks);
  assert.equal(full.disk.mount, '/data');
  assert.equal(full.pct, 90);
  assert.equal(diskUsagePct(disks[1]), 9);
});

test('machineMetrics computes CPU, memory, disk, and GPU figures', () => {
  const metrics = machineMetrics({
    cpu: { usage_pct: 71.2, cores: 32, load: [3.2, 2.8, 2.1] },
    memory: { total: 60e9, used: 29e9 },
    disks: [{ mount: '/', fs: 'ext4', total: 2.8e12, available: 2.553e12 }],
    gpus: [{ name: 'GPU 0', memory_total: 16e9, memory_used: 11e9, utilization_pct: 64 }],
  });
  assert.equal(metrics.cpuPct, 71);
  assert.equal(metrics.memPct, 48);
  assert.equal(metrics.diskPct, 9);
  assert.equal(metrics.gpus.length, 1);
  assert.equal(metrics.gpus[0].pct, 69);
});
