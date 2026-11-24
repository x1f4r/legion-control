import assert from 'node:assert/strict';
import test from 'node:test';
import {
  describePlainPermissions,
  formatActivityDuration,
  formatActivitySubtitle,
  formatCommandMeta,
  formatMachineDetailSubtitle,
  formatMachineStatusPhrase,
  formatMachinesHeaderSubtitle,
  friendlyJobName,
  friendlyJobState,
  groupActivity,
} from '../activity.js';

test('friendlyJobName hides raw commands and preserves meaningful names', () => {
  assert.equal(friendlyJobName({ name: 'Photo backup', command: 'rsync -a photos/ nas:' }), 'Photo backup');
  assert.equal(friendlyJobName({ name: 'Build', command: 'cargo build --release' }), 'Build');
  assert.equal(friendlyJobName({ name: 'proj:website:tests', command: 'npm test' }), 'Tests for website');
  assert.equal(friendlyJobName({ project: 'website', task: 'tests' }), 'Tests for website');
  assert.equal(friendlyJobName({ tool: 'power.restart' }), 'Restart');
  // Auto-generated shell/program names fall back to "Command".
  assert.equal(friendlyJobName({ name: 'sh', command: 'sh -c "sleep 30"' }), 'Command');
  assert.equal(friendlyJobName({ name: 'sleep', command: 'sleep 600' }), 'Command');
  assert.equal(friendlyJobName({ name: 'for', command: 'for i in 1 2 3; do echo tick; done' }), 'Command');
  assert.equal(friendlyJobName({ command: 'echo hi' }), 'Command');
});

test('friendlyJobState uses plain words by default and exit codes in technical mode', () => {
  assert.deepEqual(friendlyJobState({ state: 'running' }, false), { label: 'Running', cls: 'running' });
  assert.deepEqual(friendlyJobState({ state: 'succeeded', exit_code: 0 }, false), { label: 'Finished', cls: 'done' });
  assert.deepEqual(friendlyJobState({ state: 'failed', exit_code: 1 }, false), { label: 'Failed', cls: 'failed' });
  assert.deepEqual(friendlyJobState({ state: 'cancelled' }, false), { label: 'Stopped', cls: '' });
  assert.deepEqual(friendlyJobState({ state: 'interrupted' }, false), { label: 'Interrupted', cls: '' });

  assert.deepEqual(friendlyJobState({ state: 'succeeded', exit_code: 0 }, true), { label: 'exit 0', cls: 'done' });
  assert.deepEqual(friendlyJobState({ state: 'failed', exit_code: 1 }, true), { label: 'exit 1', cls: 'failed' });
  assert.deepEqual(friendlyJobState({ state: 'cancelled' }, true), { label: 'cancelled', cls: '' });
});

test('groupActivity buckets into Running now, Today, Yesterday, and Earlier', () => {
  const now = new Date('2026-06-15T14:00:00').getTime();
  const items = [
    { id: '1', state: 'running', created: new Date('2026-06-15T13:00:00').toISOString() },
    { id: '2', state: 'succeeded', finished: new Date('2026-06-15T12:00:00').toISOString() },
    { id: '3', state: 'succeeded', finished: new Date('2026-06-14T18:00:00').toISOString() },
    { id: '4', state: 'failed', finished: new Date('2026-06-10T09:00:00').toISOString() },
  ];
  const groups = groupActivity(items, now);
  assert.deepEqual(
    groups.map((g) => g.label),
    ['Running now', 'Today', 'Yesterday', 'Earlier'],
  );
  assert.equal(groups[0].items[0].id, '1');
  assert.equal(groups[1].items[0].id, '2');
  assert.equal(groups[2].items[0].id, '3');
  assert.equal(groups[3].items[0].id, '4');
});

test('formatActivityDuration and formatActivitySubtitle switch cleanly between plain and technical', () => {
  assert.equal(formatActivityDuration(3.1, false), 'took 3 seconds');
  assert.equal(formatActivityDuration(3.1, true), '3.1 s');
  assert.equal(formatActivityDuration(124, false), 'took 2 minutes');
  assert.equal(formatActivityDuration(124, true), '2 min 4 s');

  const now = Date.parse('2026-06-15T14:00:00Z');
  const job = {
    machine: 'studio',
    state: 'succeeded',
    created: '2026-06-15T12:00:00Z',
    finished: '2026-06-15T12:00:03.100Z',
    duration_s: 3.1,
  };
  assert.equal(
    formatActivitySubtitle(job, { technical: false, nowMs: now }),
    'studio · 2 hours ago · took 3 seconds',
  );
  assert.equal(
    formatActivitySubtitle(job, { technical: true, nowMs: now }),
    'studio · 2 hours ago · 3.1 s',
  );
});

test('machine status phrases and header subtitle read in plain words', () => {
  const now = 1_700_000_000_000;
  assert.equal(formatMachineStatusPhrase({ reachable: true, busy: false, runningJobs: 0 }), 'Idle');
  assert.equal(
    formatMachineStatusPhrase({ reachable: true, busy: true, runningJobs: 2 }),
    'Busy · 2 things running',
  );
  assert.equal(
    formatMachineStatusPhrase({
      reachable: true,
      busy: true,
      runningJobs: 0,
      busyReasons: [{ source: 'rule', name: 'agents', detail: '2 running' }],
    }),
    'Busy · agents',
  );
  assert.equal(
    formatMachineStatusPhrase({ reachable: false, lastSeenAt: now - 14 * 86_400_000 }, now),
    'Last on 14 days ago',
  );
  assert.equal(formatMachineStatusPhrase({ reachable: false, lastSeenAt: null }, now), 'Off');

  assert.equal(
    formatMachineDetailSubtitle({ reachable: true, busy: true, sys: { uptime_s: 11 * 3600 } }),
    'Busy · on for 11 hours',
  );
  assert.equal(
    formatMachinesHeaderSubtitle([
      { reachable: true },
      { reachable: true },
      { reachable: true },
      { reachable: false },
    ]),
    '3 are on, 1 is off',
  );
});

test('describePlainPermissions and formatCommandMeta summarize cleanly', () => {
  const tools = [
    { name: 'sys.info', effect: 'read' },
    { name: 'fs.write', effect: 'write' },
    { name: 'exec.run', effect: 'exec' },
    { name: 'power.restart', effect: 'power' },
    { name: 'device.invite', effect: 'admin' },
  ];
  assert.equal(
    describePlainPermissions(
      { role: 'operator', tools: ['sys.info', 'fs.write', 'exec.run', 'power.restart'] },
      tools,
    ),
    'May do everything except change permissions',
  );
  assert.equal(
    describePlainPermissions({ role: 'observer', tools: ['sys.info'] }, tools),
    'Look only',
  );
  assert.equal(
    describePlainPermissions(
      { role: 'owner', tools: tools.map((t) => t.name) },
      tools,
    ),
    'May do everything',
  );
  assert.equal(formatCommandMeta({ exitCode: 0, durationMs: 38 }, false), 'Finished · 38 ms');
  assert.equal(formatCommandMeta({ exitCode: 0, durationMs: 38 }, true), 'exit 0 · 38 ms');
});
