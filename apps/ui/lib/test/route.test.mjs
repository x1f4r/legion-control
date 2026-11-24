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

test('parseRoute and routeHref round-trip all views', () => {
  const cases = [
    ['', { section: 'machines', machine: null, tab: 'overview', jobId: null }, '#/machines'],
    ['#/machines', { section: 'machines', machine: null, tab: 'overview', jobId: null }, '#/machines'],
    [
      '#/machines/tower',
      { section: 'machines', machine: 'tower', tab: 'overview', jobId: null },
      '#/machines/tower',
    ],
    [
      '#/machines/tower/run',
      { section: 'machines', machine: 'tower', tab: 'run', jobId: null },
      '#/machines/tower/run',
    ],
    [
      '#/machines/tower/jobs/abc-1',
      { section: 'machines', machine: 'tower', tab: 'jobs', jobId: 'abc-1' },
      '#/machines/tower/jobs/abc-1',
    ],
    ['#/jobs', { section: 'jobs', machine: null, jobId: null }, '#/jobs'],
    [
      '#/jobs/tower/abc-1',
      { section: 'jobs', machine: 'tower', jobId: 'abc-1' },
      '#/jobs/tower/abc-1',
    ],
    ['#/tools', { section: 'tools', tool: null }, '#/tools'],
    ['#/tools/power.restart', { section: 'tools', tool: 'power.restart' }, '#/tools/power.restart'],
    ['#/device', { section: 'device' }, '#/device'],
  ];
  for (const [hash, expected, href] of cases) {
    const parsed = parseRoute(hash);
    assert.deepEqual(parsed, expected, hash);
    assert.equal(routeHref(parsed), href, hash);
  }
});

test('parseRoute falls back safely on unknown tab or route', () => {
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
