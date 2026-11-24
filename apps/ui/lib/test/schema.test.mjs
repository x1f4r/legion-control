import assert from 'node:assert/strict';
import test from 'node:test';
import {
  buildArgs,
  describeSchema,
  groupToolsByFamily,
  hasDryRun,
  initialValues,
  needsConfirmation,
  parseRawArgs,
  valuesFromArgs,
} from '../schema.js';

const SAMPLE_SCHEMA = {
  type: 'object',
  required: ['path'],
  properties: {
    path: { type: 'string', description: 'Directory to list.' },
    depth: { type: 'integer', default: 1, minimum: 1, maximum: 8 },
    hidden: { type: 'boolean', default: true },
    direction: { type: 'string', enum: ['status', 'push', 'pull'], default: 'status' },
    tags: { type: 'array', items: { type: 'string' } },
    options: {
      type: 'object',
      properties: {
        retry: { type: 'integer', minimum: 0 },
        strict: { type: 'boolean', default: false },
      },
    },
    env: { type: 'object', additionalProperties: { type: 'string' } },
  },
};

test('describeSchema classifies supported field types', () => {
  const fields = describeSchema(SAMPLE_SCHEMA);
  const byName = Object.fromEntries(fields.map((f) => [f.name, f]));
  assert.equal(byName.path.kind, 'string');
  assert.equal(byName.path.required, true);
  assert.equal(byName.depth.kind, 'integer');
  assert.equal(byName.depth.defaultValue, 1);
  assert.equal(byName.hidden.kind, 'boolean');
  assert.equal(byName.direction.kind, 'enum');
  assert.deepEqual(byName.direction.enumValues, ['status', 'push', 'pull']);
  assert.equal(byName.tags.kind, 'string-array');
  assert.equal(byName.options.kind, 'object');
  assert.equal(byName.options.fields.length, 2);
  assert.equal(byName.env.kind, 'json');
});

test('buildArgs validates required fields, ranges, arrays, nested objects, and JSON', () => {
  const empty = buildArgs(SAMPLE_SCHEMA, initialValues(SAMPLE_SCHEMA));
  assert.equal(empty.ok, false);
  assert.ok(empty.errors.path);

  const values = {
    ...initialValues(SAMPLE_SCHEMA),
    path: '/home/me',
    depth: '3',
    hidden: false,
    direction: 'push',
    tags: 'one\n two \n\nthree',
    options: { retry: '2', strict: true },
    env: '{"FOO":"bar"}',
  };
  const built = buildArgs(SAMPLE_SCHEMA, values);
  assert.equal(built.ok, true, JSON.stringify(built.errors));
  assert.deepEqual(built.args, {
    path: '/home/me',
    depth: 3,
    hidden: false,
    direction: 'push',
    tags: ['one', 'two', 'three'],
    options: { retry: 2, strict: true },
    env: { FOO: 'bar' },
  });

  const roundTrip = valuesFromArgs(SAMPLE_SCHEMA, built.args);
  assert.equal(roundTrip.path, '/home/me');
  assert.equal(roundTrip.tags, 'one\ntwo\nthree');
  assert.equal(roundTrip.options.retry, '2');
  assert.equal(roundTrip.options.strict, true);

  const badRange = buildArgs(SAMPLE_SCHEMA, { ...initialValues(SAMPLE_SCHEMA), path: '/', depth: '99' });
  assert.equal(badRange.ok, false);
  assert.ok(badRange.errors.depth);

  const badJson = buildArgs(SAMPLE_SCHEMA, { ...initialValues(SAMPLE_SCHEMA), path: '/', env: '{bad' });
  assert.equal(badJson.ok, false);
  assert.ok(badJson.errors.env);
});

test('parseRawArgs validates JSON object input', () => {
  assert.deepEqual(parseRawArgs(''), { ok: true, args: {}, error: null });
  assert.deepEqual(parseRawArgs('{"a": 1}'), { ok: true, args: { a: 1 }, error: null });
  assert.equal(parseRawArgs('[1,2]').ok, false);
  assert.equal(parseRawArgs('{').ok, false);
});

test('hasDryRun, needsConfirmation, and groupToolsByFamily', () => {
  assert.equal(hasDryRun({ properties: { dry_run: { type: 'boolean' } } }), true);
  assert.equal(hasDryRun({ properties: {} }), false);
  assert.equal(needsConfirmation('destroy'), true);
  assert.equal(needsConfirmation('power'), true);
  assert.equal(needsConfirmation('exec'), false);

  const tools = [
    { name: 'power.restart', effect: 'power', description: 'Restart this machine. More details.' },
    { name: 'fs.list', effect: 'read', description: 'List directory entries.' },
    { name: 'fs.read', effect: 'read', description: 'Read a file.' },
  ];
  const groups = groupToolsByFamily(
    tools,
    '',
    ['fs.list', 'fs.read'],
    { principal: 'phone', role: 'observer' },
  );
  assert.equal(groups.length, 2);
  assert.equal(groups[0].family, 'fs');
  assert.equal(groups[0].tools.length, 2);
  assert.equal(groups[0].tools[0].allowed, true);
  assert.equal(groups[1].family, 'power');
  assert.equal(groups[1].tools[0].allowed, false);
  assert.match(groups[1].tools[0].reason, /observer/);

  const filtered = groupToolsByFamily(tools, 'restart', ['fs.list'], { role: 'observer' });
  assert.equal(filtered.length, 1);
  assert.equal(filtered[0].tools[0].name, 'power.restart');
});
