import assert from 'node:assert/strict';
import test from 'node:test';
import { buildActionGroups, findActionById } from '../actions.js';

const SAMPLE_TOOLS = [
  { name: 'power.sleep', effect: 'power' },
  { name: 'power.wake', effect: 'power' },
  { name: 'power.restart', effect: 'power' },
  { name: 'power.shutdown', effect: 'power' },
  { name: 'fs.list', effect: 'read' },
  { name: 'fs.transfer', effect: 'write' },
  { name: 'exec.run', effect: 'exec' },
  { name: 'meta.update', effect: 'admin' },
  { name: 'proj.list', effect: 'read' },
  { name: 'svc.list', effect: 'read' },
  { name: 'fleet.ping', effect: 'read' },
  { name: 'fleet.add', effect: 'admin' },
];

test('buildActionGroups hides groups with no allowed actions and marks partial disallowed ones cleanly', () => {
  const allowed = new Set(['power.sleep', 'power.wake', 'power.restart', 'power.shutdown', 'fs.list', 'exec.run']);
  const groups = buildActionGroups({
    tools: SAMPLE_TOOLS,
    allowedSet: allowed,
    whoami: { role: 'operator' },
  });
  assert.deepEqual(
    groups.map((g) => g.group),
    ['Power', 'Files', 'Run'],
  );
  const runGroup = groups.find((g) => g.group === 'Run');
  const update = runGroup.items.find((i) => i.id === 'update');
  assert.equal(update.allowed, false);
  assert.equal(update.reason, 'Only an owner can do this');

  const none = buildActionGroups({
    tools: SAMPLE_TOOLS,
    allowedSet: new Set(['meta.whoami']),
    whoami: { role: 'guest' },
  });
  assert.deepEqual(none, []);
});

test('buildActionGroups filters by plain words and by tool name in technical mode', () => {
  const plain = buildActionGroups({
    tools: SAMPLE_TOOLS,
    query: 'meta.update',
    technical: false,
  });
  assert.equal(plain.length, 0);

  const tech = buildActionGroups({
    tools: SAMPLE_TOOLS,
    query: 'meta.update',
    technical: true,
  });
  assert.equal(tech.length, 1);
  assert.equal(tech[0].items[0].id, 'update');

  const sleep = findActionById('sleep', SAMPLE_TOOLS);
  assert.equal(sleep?.tool, 'power.sleep');
});
