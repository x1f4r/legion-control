import assert from 'node:assert/strict';
import test from 'node:test';
import {
  describeDryRunSummary,
  describePlainPermissions,
  describeRolePlain,
  formatActivityDuration,
  formatActivitySubtitle,
  formatCommandMeta,
  formatDeviceTransferSubtitle,
  formatDeviceTransferTitle,
  formatInviteValidity,
  formatMachineDetailSubtitle,
  formatMachineStatusPhrase,
  formatMachinesHeaderSubtitle,
  formatMachinesShortFact,
  formatTransferEta,
  formatTransferJobSubtitle,
  friendlyDeviceTransferState,
  friendlyJobName,
  friendlyJobState,
  groupActivity,
  plainRefusalParts,
  transferProgressPct,
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
  assert.equal(
    formatMachinesShortFact([
      { reachable: true },
      { reachable: true },
      { reachable: true },
      { reachable: false },
    ]),
    '3 on, 1 off',
  );
  assert.equal(
    formatMachinesShortFact([{ reachable: true }, { reachable: true }]),
    '2 on',
  );
  assert.equal(
    formatMachinesShortFact([{ reachable: false }, { reachable: false }]),
    '2 off',
  );
  assert.equal(formatMachinesShortFact([]), '');
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

test('wakePending phrases, dry-run summaries, refusal splitting, and invite copy read plainly', () => {
  assert.equal(
    formatMachineStatusPhrase({ reachable: false, lastSeenAt: null }, Date.now(), { wakePending: true }),
    'Wake sent · waiting for it to answer',
  );
  assert.equal(
    formatMachineDetailSubtitle({ reachable: false, lastSeenAt: null }, Date.now(), { wakePending: true }),
    'Wake sent · waiting for it to answer',
  );
  assert.equal(
    formatMachineStatusPhrase({ reachable: true, busy: false, runningJobs: 0 }, Date.now(), { wakePending: true }),
    'Idle',
  );

  assert.equal(describeDryRunSummary('power.sleep', 'tower'), 'tower would go to sleep.');
  assert.equal(
    describeDryRunSummary('power.restart', 'tower', { into: 'windows' }),
    'tower would restart into Windows.',
  );
  assert.equal(describeDryRunSummary('power.shutdown', 'tower'), 'tower would switch off.');
  assert.equal(
    describeDryRunSummary('power.wake', 'tower', { machine: 'laptop' }),
    'A wake signal would be sent to laptop.',
  );

  const busyCheckRefusal = plainRefusalParts({
    message: 'node.toml does not say what keeps this machine busy. Add busy = "none" or a probe.',
    hint: 'Edit node.toml to configure busy detection.',
  });
  assert.equal(busyCheckRefusal.first, 'this machine does not have a busy check set up yet.');
  assert.match(busyCheckRefusal.detailMessage, /node\.toml/);
  assert.match(busyCheckRefusal.detailHint, /busy detection/);

  const genericRefusal = plainRefusalParts({
    message: 'Refused to power.sleep: agents is busy. Wait for it to finish.',
    hint: 'Pass force = true to override.',
  });
  assert.equal(genericRefusal.first, 'agents is busy.');
  assert.match(genericRefusal.detailMessage, /Wait for it to finish/);

  assert.equal(describeRolePlain('owner'), 'do everything on this machine');
  assert.equal(describeRolePlain('operator'), 'do everything except change permissions');
  assert.equal(describeRolePlain('observer'), 'look only');
  assert.equal(formatInviteValidity(15), '15 minutes');
  assert.equal(formatInviteValidity(60), '1 hour');
  assert.equal(formatInviteValidity(1440), '1 day');
});

test('transfer job and device transfer formatting uses plain words by default and paths in technical mode', () => {
  assert.equal(transferProgressPct({ bytes_done: 498_000_000, bytes_total: 1_200_000_000 }), 42);
  assert.equal(formatTransferEta(498_000_000, 1_200_000_000, 12_000_000), 'about 1 minute left');

  const runningTransfer = {
    kind: 'transfer',
    title: 'Copying Arrival.mp4 to pi',
    machine: 'tower',
    state: 'running',
    progress: { bytes_done: 498_000_000, bytes_total: 1_200_000_000, rate: 12_000_000 },
  };
  assert.equal(friendlyJobName(runningTransfer), 'Copying Arrival.mp4 to pi');
  assert.match(formatTransferJobSubtitle(runningTransfer, { technical: false }), /tower .* 1\.2 GB .* about 1 minute left/);

  const doneDl = {
    id: 'dt-1',
    direction: 'download',
    machine: 'tower',
    path: '/home/me/Tickets.pdf',
    localPath: 'Downloads/Legion/Tickets.pdf',
    name: 'Tickets.pdf',
    state: 'done',
    bytes_done: 310_000,
    bytes_total: 310_000,
    hash: 'blake3:88f12c09aa7b34e2c51d0e89',
    canOpen: true,
  };
  assert.equal(formatDeviceTransferTitle(doneDl, false), 'Downloaded Tickets.pdf');
  assert.equal(formatDeviceTransferSubtitle(doneDl, { deviceNoun: 'phone', technical: false }), 'To this phone · tap to open');
  assert.deepEqual(friendlyDeviceTransferState(doneDl, false), { label: 'Finished', cls: 'done' });
  assert.match(formatDeviceTransferTitle(doneDl, true), /tower:\/home\/me\/Tickets\.pdf/);
  assert.match(formatDeviceTransferSubtitle(doneDl, { technical: true }), /310000 B/);

  const runningDl = {
    direction: 'download',
    name: 'Beach.mp4',
    state: 'running',
    bytes_done: 210_000_000,
    bytes_total: 840_000_000,
  };
  assert.equal(formatDeviceTransferTitle(runningDl, false), 'Downloading Beach.mp4');
  assert.deepEqual(friendlyDeviceTransferState(runningDl, false), { label: '25%', cls: 'running' });

  const cancellingDl = {
    direction: 'download',
    name: 'Beach.mp4',
    state: 'cancelling',
    error: { code: 'failed', message: 'Could not remove Beach.mp4: file in use.' },
  };
  assert.equal(formatDeviceTransferTitle(cancellingDl, false), 'Cancelling Beach.mp4');
  assert.equal(
    formatDeviceTransferSubtitle(cancellingDl, { deviceNoun: 'computer', technical: false }),
    'Could not remove Beach.mp4: file in use.',
  );
  assert.deepEqual(friendlyDeviceTransferState(cancellingDl, false), { label: 'Cancelling', cls: 'failed' });
  assert.deepEqual(friendlyDeviceTransferState({ ...cancellingDl, error: null }, false), {
    label: 'Cancelling',
    cls: '',
  });
  assert.equal(
    formatDeviceTransferSubtitle({ ...cancellingDl, error: null }, { deviceNoun: 'phone', technical: false }),
    'Removing partial files…',
  );
  const cancellingProtocolErr = {
    ...cancellingDl,
    error: { code: 'unknown_tool', message: 'Unknown tool: fs.remove', machine: 'studio' },
  };
  const plainCancelErr = formatDeviceTransferSubtitle(cancellingProtocolErr, {
    deviceNoun: 'phone',
    technical: false,
  });
  assert.ok(!plainCancelErr.includes('fs.remove'), `Cancelling error must use plain formatting: ${plainCancelErr}`);
  assert.match(
    formatDeviceTransferSubtitle(cancellingProtocolErr, { deviceNoun: 'phone', technical: true }),
    /fs\.remove/,
  );
});
