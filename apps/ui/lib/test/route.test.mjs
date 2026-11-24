import assert from 'node:assert/strict';
import test from 'node:test';
import { extractHashToken, parseRoute, routeHref } from '../route.js';

test('extractHashToken reads #token= once and clears it', () => {
  assert.deepEqual(extractHashToken('#token=lgn_abc123'), {
    token: 'lgn_abc123',
    cleanHash: '',
  });
  assert.deepEqual(extractHashToken('#token=lgn_a%2Bb&next=1'), {
    token: 'lgn_a+b',
    cleanHash: '',
  });
  assert.deepEqual(extractHashToken('#/machines/tower'), {
    token: null,
    cleanHash: '#/machines/tower',
  });
});

test('parseRoute and routeHref round-trip all v2 views', () => {
  const cases = [
    ['', { section: 'machines', machine: null, tab: 'overview', jobId: null }, '#/machines'],
    ['#/machines', { section: 'machines', machine: null, tab: 'overview', jobId: null }, '#/machines'],
    [
      '#/machines/tower',
      { section: 'machines', machine: 'tower', tab: 'overview', jobId: null },
      '#/machines/tower',
    ],
    [
      '#/machines/tower/command',
      { section: 'machines', machine: 'tower', tab: 'command', jobId: null },
      '#/machines/tower/command',
    ],
    [
      '#/machines/tower/more',
      { section: 'machines', machine: 'tower', tab: 'more', jobId: null },
      '#/machines/tower/more',
    ],
    [
      '#/machines/tower/activity/abc-1',
      { section: 'machines', machine: 'tower', tab: 'activity', jobId: 'abc-1' },
      '#/machines/tower/activity/abc-1',
    ],
    ['#/activity', { section: 'activity', machine: null, jobId: null }, '#/activity'],
    [
      '#/activity/tower/abc-1',
      { section: 'activity', machine: 'tower', jobId: 'abc-1' },
      '#/activity/tower/abc-1',
    ],
    ['#/actions', { section: 'actions', allTools: false, tool: null, action: null }, '#/actions'],
    [
      '#/actions/sleep',
      { section: 'actions', allTools: false, tool: null, action: 'sleep' },
      '#/actions/sleep',
    ],
    [
      '#/actions/all',
      { section: 'actions', allTools: true, tool: null, action: null },
      '#/actions/all',
    ],
    [
      '#/actions/all/power.restart',
      { section: 'actions', allTools: true, tool: 'power.restart', action: null },
      '#/actions/all/power.restart',
    ],
    ['#/settings', { section: 'settings', page: null, sub: null, item: null }, '#/settings'],
    [
      '#/settings/appearance',
      { section: 'settings', page: 'appearance', sub: null, item: null },
      '#/settings/appearance',
    ],
    [
      '#/settings/connections/invite',
      { section: 'settings', page: 'connections', sub: 'invite', item: null },
      '#/settings/connections/invite',
    ],
    [
      '#/settings/connections/node/tower',
      { section: 'settings', page: 'connections', sub: 'node', item: 'tower' },
      '#/settings/connections/node/tower',
    ],
  ];
  for (const [hash, expected, href] of cases) {
    const parsed = parseRoute(hash);
    assert.deepEqual(parsed, expected, hash);
    assert.equal(routeHref(parsed), href, hash);
  }
});

test('parseRoute accepts legacy aliases and falls back safely', () => {
  assert.deepEqual(parseRoute('#/jobs/tower/j-1'), {
    section: 'activity',
    machine: 'tower',
    jobId: 'j-1',
  });
  assert.deepEqual(parseRoute('#/tools/meta.version'), {
    section: 'actions',
    allTools: true,
    tool: 'meta.version',
    action: null,
  });
  assert.deepEqual(parseRoute('#/device'), {
    section: 'settings',
    page: 'device',
    sub: null,
    item: null,
  });
  assert.deepEqual(parseRoute('#/machines/tower/run'), {
    section: 'machines',
    machine: 'tower',
    tab: 'command',
    jobId: null,
  });
  assert.deepEqual(parseRoute('#/machines/pi/nonexistent'), {
    section: 'machines',
    machine: 'pi',
    tab: 'overview',
    jobId: null,
  });
  assert.deepEqual(parseRoute('#/unknown'), {
    section: 'machines',
    machine: null,
    tab: 'overview',
    jobId: null,
  });
});
