import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  computeFileCapabilities,
  createCapabilitiesStore,
  describeMissingFeatures,
  isNeedsNewerLegionError,
  makeNeedsNewerLegionError,
  parseUnknownFieldError,
} from '../capabilities.js';
import {
  containsTechnicalText,
  formatErrorForDisplay,
  isTerminalHint,
  plainErrorMessage,
  plainMessageForCode,
} from '../errors.js';
import { createMockLink } from '../../link/mock.js';
import {
  OLD_FIXTURE_BUILD,
  OLD_FIXTURE_TOOLS,
  OLD_FIXTURE_VERSION,
  validateToolArgsAgainstSpec,
} from '../../link/mock-old-tools.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURE_PATH = path.resolve(__dirname, '../../test/fixtures/tools-05ba1ef2e385.json');

const NEW_TOOLS = [
  {
    name: 'fs.list',
    effect: 'read',
    input_schema: {
      properties: { path: {}, depth: {}, hidden: {}, limit: {}, sort: {}, order: {}, offset: {}, categories: {} },
    },
  },
  {
    name: 'fs.read',
    effect: 'read',
    input_schema: { properties: { path: {}, offset: {}, length: {}, encoding: {} } },
  },
  {
    name: 'fs.find',
    effect: 'read',
    input_schema: {
      properties: {
        path: {},
        glob: {},
        kind: {},
        name: {},
        categories: {},
        min_size: {},
        max_size: {},
        modified_after: {},
        limit: {},
      },
    },
  },
  { name: 'fs.tags', effect: 'read', input_schema: { properties: { paths: {} } } },
  { name: 'fs.thumb', effect: 'read', input_schema: { properties: { paths: {}, size: { maximum: 1024 }, fit: {} } } },
  {
    name: 'fs.transfer',
    effect: 'write',
    input_schema: { properties: { from: {}, to: {}, dry_run: {}, background: {}, move: {}, if_exists: {} } },
  },
  { name: 'fav.list', effect: 'read', input_schema: { properties: {} } },
  { name: 'fav.add', effect: 'write', input_schema: { properties: { machine: {}, path: {} } } },
  { name: 'fav.remove', effect: 'write', input_schema: { properties: { id: {} } } },
  { name: 'sync.list', effect: 'read', input_schema: { properties: {} } },
  { name: 'sys.info', effect: 'read', input_schema: { properties: {} } },
  { name: 'sys.procs', effect: 'read', input_schema: { properties: { filter: {}, sort: {}, limit: {}, group: {} } } },
  { name: 'sys.kill', effect: 'destroy', input_schema: { properties: { pid: {}, start_time: {}, targets: {}, tree: {}, force: {} } } },
  { name: 'update.list', effect: 'read', input_schema: { properties: {} } },
];

test('05ba1ef fixture matches mock-old-tools.js verbatim (78 tools, schemas, and strict arg validator)', () => {
  const raw = JSON.parse(fs.readFileSync(FIXTURE_PATH, 'utf8'));
  assert.equal(Array.isArray(raw.tools), true);
  assert.equal(raw.tools.length, 78);
  assert.equal(OLD_FIXTURE_TOOLS.length, 78);
  assert.deepEqual(
    OLD_FIXTURE_TOOLS.map((t) => t.name),
    raw.tools.map((t) => t.name),
  );
  for (let i = 0; i < raw.tools.length; i += 1) {
    assert.equal(OLD_FIXTURE_TOOLS[i].effect, raw.tools[i].effect);
    assert.deepEqual(
      Object.keys(OLD_FIXTURE_TOOLS[i].input_schema?.properties || {}).sort(),
      Object.keys(raw.tools[i].input_schema?.properties || {}).sort(),
    );
  }

  // Strict validator rejects unknown fields with the exact real-node error and CLI hint.
  const listSpec = OLD_FIXTURE_TOOLS.find((t) => t.name === 'fs.list');
  assert.throws(
    () => validateToolArgsAgainstSpec('studio', listSpec, { path: '~', sort: 'name' }),
    (err) => {
      assert.equal(err.code, 'invalid_args');
      assert.equal(err.machine, 'studio');
      assert.match(err.message, /fs\.list: `sort`: unknown field `sort`, expected one of/);
      assert.equal(err.hint, '`legion help fs.list` shows the arguments it takes.');
      return true;
    },
  );
  // Valid args pass.
  assert.doesNotThrow(() => validateToolArgsAgainstSpec('studio', listSpec, { path: '~', depth: 1, hidden: false, limit: 500 }));
});

test('capabilities store assumes the least when unknown, deduplicates concurrent fetches, and refetches on build change or reconnect', async () => {
  let toolsCalls = 0;
  let currentBuild = OLD_FIXTURE_BUILD;
  let currentTools = OLD_FIXTURE_TOOLS;

  const link = {
    kind: 'app',
    async tools({ on }) {
      toolsCalls += 1;
      await new Promise((r) => setTimeout(r, 10));
      if (on === 'studio') return currentTools;
      return NEW_TOOLS;
    },
    async call(tool, _args, { on }) {
      if (tool === 'meta.whoami') {
        return { node: on, principal: 'me', role: 'owner', tools: (on === 'studio' ? currentTools : NEW_TOOLS).map((t) => t.name) };
      }
      if (tool === 'meta.version') {
        return on === 'studio'
          ? { version: OLD_FIXTURE_VERSION, build: currentBuild }
          : { version: '0.2.0', build: 'new0001' };
      }
      return {};
    },
    contentUrl: async () => ({ url: 'data:text/plain,' }),
    download: async () => ({ id: 'dl1' }),
  };

  const store = createCapabilitiesStore(link);

  // Before loading: everything is conservative (false).
  assert.equal(store.isKnown('studio'), false);
  assert.equal(store.hasTool('studio', 'fs.list'), false);
  assert.equal(store.acceptsArg('studio', 'fs.list', 'sort'), false);
  const unknownCaps = store.fileCapabilities('studio');
  assert.equal(unknownCaps.known, false);
  assert.equal(unknownCaps.hasList, false);
  assert.equal(unknownCaps.hasListSort, false);
  assert.equal(unknownCaps.hasThumb, false);
  assert.equal(unknownCaps.hasContentUrl, false);

  // Ten concurrent callers asking for the same machine trigger only ONE `link.tools` call.
  await Promise.all(Array.from({ length: 10 }, () => store.ensure('studio')));
  assert.equal(toolsCalls, 1);
  assert.equal(store.isKnown('studio'), true);
  assert.equal(store.hasTool('studio', 'fs.list'), true);
  assert.equal(store.acceptsArg('studio', 'fs.list', 'path'), true);
  assert.equal(store.acceptsArg('studio', 'fs.list', 'sort'), false);
  assert.equal(store.hasTool('studio', 'fs.thumb'), false);
  assert.equal(store.hasTool('studio', 'fav.list'), false);

  const oldCaps = store.fileCapabilities('studio');
  assert.equal(oldCaps.known, true);
  assert.equal(oldCaps.hasList, true);
  assert.equal(oldCaps.hasListSort, false);
  assert.equal(oldCaps.hasListOffset, false);
  assert.equal(oldCaps.hasThumb, false);
  assert.equal(oldCaps.hasFav, false);
  assert.equal(oldCaps.hasContentUrl, false);
  assert.equal(oldCaps.hasDownload, true); // app link still supports download fallback

  const missing = store.missingFeatures('studio');
  assert.ok(missing.some((m) => m.includes('fs.list')));
  assert.ok(missing.includes('fs.thumb'));
  assert.ok(missing.includes('fav.*'));
  assert.ok(missing.includes('sys.procs (group)'));
  assert.ok(missing.includes('sys.kill (targets)'));
  assert.ok(missing.includes('update.*'));

  // Current machine in the same store has full capabilities and no missing features.
  await store.ensure('tower');
  assert.equal(store.acceptsArg('tower', 'fs.list', 'sort'), true);
  assert.equal(store.hasTool('tower', 'fs.thumb'), true);
  assert.deepEqual(store.missingFeatures('tower'), []);

  // Build change invalidates ONLY the upgraded machine.
  currentBuild = 'upgraded9999';
  currentTools = NEW_TOOLS;
  const invalidated = store.syncMachineMeta('studio', { version: '0.2.0', build: currentBuild, reachable: true });
  assert.equal(invalidated, true);
  assert.equal(store.isKnown('studio'), false);
  assert.equal(store.isKnown('tower'), true);

  await store.ensure('studio');
  assert.equal(store.acceptsArg('studio', 'fs.list', 'sort'), true);
  assert.deepEqual(store.missingFeatures('studio'), []);

  // Disconnect then reconnect invalidates the reconnected machine so it is refetched.
  store.syncMachineMeta('studio', { reachable: false });
  assert.equal(store.isKnown('studio'), true);
  const reconnected = store.syncMachineMeta('studio', { reachable: true });
  assert.equal(reconnected, true);
  assert.equal(store.isKnown('studio'), false);
  store.dispose();
});

test('guarded call drops undeclared refinements, blocks absent tools/essential args, times out boundedly, and retries once on invalid_args', async () => {
  const calls = [];
  const link = {
    kind: 'app',
    async tools({ on }) {
      if (on === 'slow') {
        return new Promise(() => {});
      }
      if (on === 'stale') {
        // Claims `fs.list` accepts `sort` and `offset`, but the node will reject `sort` at runtime.
        return [
          {
            name: 'fs.list',
            effect: 'read',
            input_schema: { properties: { path: {}, depth: {}, hidden: {}, limit: {}, sort: {}, order: {}, offset: {} } },
          },
        ];
      }
      return OLD_FIXTURE_TOOLS;
    },
    async call(tool, args, { on }) {
      if (tool === 'meta.whoami') return { node: on, principal: 'me', role: 'owner' };
      if (tool === 'meta.version') return { version: OLD_FIXTURE_VERSION, build: OLD_FIXTURE_BUILD };
      calls.push({ on, tool, args: { ...args } });
      if (on === 'stale' && tool === 'fs.list' && ('sort' in args || 'order' in args || 'offset' in args)) {
        throw {
          code: 'invalid_args',
          machine: 'stale',
          message:
            'fs.list: `sort`: unknown field `sort`, expected one of `path`, `depth`, `hidden`, `limit`.',
          hint: '`legion help fs.list` shows the arguments it takes.',
        };
      }
      return { path: args.path || '~', entries: [{ name: 'a.txt', path: '/home/me/a.txt', kind: 'file' }] };
    },
  };

  const store = createCapabilitiesStore(link, { timeoutMs: 60 });

  // 1. Optional refinements (`sort`, `order`, `offset`, `categories`) are dropped before calling.
  const droppedHook = [];
  const res = await store.call(
    'fs.list',
    { path: '~', depth: 1, hidden: false, limit: 200, sort: 'size', order: 'desc', offset: 0, categories: ['image'] },
    {
      on: 'studio',
      optional: ['sort', 'order', 'offset', 'categories'],
      onDrop(sentArgs, dropped) {
        droppedHook.push(...dropped);
        if (dropped.includes('offset')) sentArgs.limit = 5000;
      },
    },
  );
  assert.deepEqual(res.dropped.sort(), ['categories', 'offset', 'order', 'sort']);
  assert.deepEqual(droppedHook.sort(), ['categories', 'offset', 'order', 'sort']);
  assert.deepEqual(calls[0], {
    on: 'studio',
    tool: 'fs.list',
    args: { path: '~', depth: 1, hidden: false, limit: 5000 },
  });

  // 2. Absent tool fails without calling with a typed `needs_newer_legion` error.
  const callsBeforeAbsent = calls.length;
  await assert.rejects(
    () => store.call('fs.thumb', { paths: ['/home/me/a.jpg'] }, { on: 'studio' }),
    (err) => {
      assert.equal(isNeedsNewerLegionError(err), true);
      assert.equal(err.code, 'needs_newer_legion');
      assert.equal(err.machine, 'studio');
      assert.equal(err.tool, 'fs.thumb');
      return true;
    },
  );
  assert.equal(calls.length, callsBeforeAbsent);

  // 3. Undeclared essential argument fails without calling with typed `needs_newer_legion`.
  await assert.rejects(
    () => store.call('fs.transfer', { from: 'studio:~/a', to: 'tower:~/a', move: true }, { on: 'studio', optional: ['background'] }),
    (err) => {
      assert.equal(isNeedsNewerLegionError(err), true);
      assert.deepEqual(err.missingArgs, ['move']);
      return true;
    },
  );
  assert.equal(calls.length, callsBeforeAbsent);

  // 4. Unresponsive machine times out boundedly as `unreachable`.
  await assert.rejects(
    () => store.call('fs.list', { path: '~' }, { on: 'slow', timeoutMs: 40 }),
    { code: 'unreachable' },
  );

  // 5. Second line of defence: stale cache claims `sort`/`order`/`offset` are accepted, node returns
  // `invalid_args` naming `sort` + expected fields -> drops them, updates cached caps, retries once.
  const staleRes = await store.call(
    'fs.list',
    { path: '~', limit: 200, sort: 'name', order: 'asc', offset: 0 },
    { on: 'stale', optional: ['sort', 'order', 'offset'] },
  );
  assert.equal(staleRes.value.entries.length, 1);
  assert.deepEqual(staleRes.dropped.sort(), ['offset', 'order', 'sort']);
  assert.equal(store.acceptsArg('stale', 'fs.list', 'sort'), false);
  assert.equal(store.acceptsArg('stale', 'fs.list', 'order'), false);
  assert.equal(store.acceptsArg('stale', 'fs.list', 'offset'), false);

  store.dispose();
});

test('errors.js hides CLI hints, backticks, config paths, and dotted tool names when Technical details is off', () => {
  assert.equal(isTerminalHint('`legion help fs.list` shows the arguments it takes.'), true);
  assert.equal(isTerminalHint('Edit policy.toml to allow this.'), true);
  assert.equal(isTerminalHint('Run legion token.add phone phone'), true);
  assert.equal(isTerminalHint('Try again in a moment.'), false);

  assert.equal(containsTechnicalText('fs.list: `sort`: unknown field `sort`'), true);
  assert.equal(containsTechnicalText('Principal "phone" (role "observer") may not call fs.remove'), true);
  assert.equal(containsTechnicalText('Could not reach studio.'), false);

  const realBugErr = {
    code: 'invalid_args',
    machine: 'studio',
    message:
      'fs.list: `sort`: unknown field `sort`, expected one of `path`, `depth`, `hidden`, `limit`.',
    hint: '`legion help fs.list` shows the arguments it takes.',
  };

  // Plain mode: no CLI hint, no backticks, no dotted tool name.
  const plain = formatErrorForDisplay({ error: realBugErr, technical: false });
  assert.equal(plain.heading, 'This machine did not understand the request. It may be running an older Legion.');
  assert.equal(plain.detail, null);
  assert.equal(plain.machinePrefix, '');
  assert.ok(!plain.heading.includes('fs.'));
  assert.ok(!plain.heading.includes('`'));

  // Technical mode: full raw message, machine prefix, and hint.
  const tech = formatErrorForDisplay({ error: realBugErr, technical: true });
  assert.equal(tech.machinePrefix, 'studio: ');
  assert.equal(tech.heading, realBugErr.message);
  assert.equal(tech.detail, realBugErr.hint);

  // Typed needs_newer_legion error.
  const olderErr = makeNeedsNewerLegionError({ machine: 'studio', tool: 'fs.thumb' });
  const olderPlain = formatErrorForDisplay({ error: olderErr, technical: false });
  assert.equal(olderPlain.heading, 'studio needs a newer Legion for this.');
  assert.equal(olderPlain.detail, null);

  // parseUnknownFieldError extracts unknown field and expected list.
  const parsed = parseUnknownFieldError(realBugErr);
  assert.deepEqual(parsed, { field: 'sort', expected: ['path', 'depth', 'hidden', 'limit'] });

  // plainErrorMessage sanitizes toasts in plain mode.
  assert.equal(
    plainErrorMessage(realBugErr, 'Fallback.', { technical: false }),
    plainMessageForCode('invalid_args'),
  );
  assert.equal(
    plainErrorMessage({ code: 'denied', message: 'Principal "phone" (role "observer") may not call fs.remove' }, 'Fallback.', {
      technical: false,
      deviceNoun: 'phone',
    }),
    'This phone is not allowed to do that here.',
  );
});

test('mixed mock fleet returns current tools for tower and exact 05ba1ef tools with strict rejection for studio', async () => {
  const link = createMockLink('multi,mixed');
  const store = createCapabilitiesStore(link);

  await Promise.all([store.ensure('tower'), store.ensure('studio')]);
  assert.equal(store.acceptsArg('tower', 'fs.list', 'sort'), true);
  assert.equal(store.hasTool('tower', 'fs.thumb'), true);
  assert.equal(store.acceptsArg('studio', 'fs.list', 'sort'), false);
  assert.equal(store.hasTool('studio', 'fs.thumb'), false);
  assert.equal(store.hasTool('studio', 'fav.list'), false);

  // Raw unguarded call to studio with `sort` is strictly rejected with invalid_args.
  await assert.rejects(
    () => link.call('fs.list', { path: '~', sort: 'size' }, { on: 'studio' }),
    (err) => err?.code === 'invalid_args' && /unknown field `sort`/.test(err.message),
  );

  // Guarded call succeeds cleanly by dropping optional refinements.
  const res = await store.call(
    'fs.list',
    { path: '~', depth: 1, hidden: false, limit: 200, sort: 'size', order: 'desc', offset: 0 },
    { on: 'studio', optional: ['sort', 'order', 'offset', 'categories'] },
  );
  assert.ok(Array.isArray(res.value.entries));
  assert.ok(res.value.entries.length > 0);
  assert.deepEqual(res.dropped.sort(), ['offset', 'order', 'sort']);

  // computeFileCapabilities conservative default when unknown.
  const emptyCaps = computeFileCapabilities({ known: false, tools: NEW_TOOLS, link });
  assert.equal(emptyCaps.known, false);
  assert.equal(emptyCaps.hasList, false);
  assert.equal(emptyCaps.hasTags, false);
  assert.equal(emptyCaps.hasAudioThumb, false);
  assert.equal(emptyCaps.thumbMaxSize, 0);
  assert.equal(describeMissingFeatures(store.get('tower')).length, 0);
  assert.ok(describeMissingFeatures(store.get('studio')).length > 0);

  store.dispose();
});

test('capabilities store exposes hasTags, hasAudioThumb, and thumbMaxSize and treats missing fs.tags silently', async () => {
  const PRE_TAGS_TOOLS = NEW_TOOLS.filter((t) => t.name !== 'fs.tags').map((t) =>
    t.name === 'fs.thumb'
      ? { ...t, input_schema: { properties: { paths: {}, size: { maximum: 512 }, fit: {} } } }
      : t,
  );
  const link = {
    kind: 'app',
    async tools({ on }) {
      if (on === 'current') return NEW_TOOLS;
      if (on === 'pretags') return PRE_TAGS_TOOLS;
      return OLD_FIXTURE_TOOLS;
    },
    async call(tool, _args, { on }) {
      if (tool === 'meta.whoami') {
        const list = on === 'current' ? NEW_TOOLS : on === 'pretags' ? PRE_TAGS_TOOLS : OLD_FIXTURE_TOOLS;
        return { node: on, principal: 'me', role: 'owner', tools: list.map((t) => t.name) };
      }
      if (tool === 'meta.version') {
        return { version: '0.2.0', build: `b-${on}` };
      }
      return {};
    },
    contentUrl: async () => ({ url: 'data:text/plain,' }),
  };

  const store = createCapabilitiesStore(link);
  await Promise.all([store.ensure('current'), store.ensure('pretags'), store.ensure('legacy')]);

  const currentCaps = store.fileCapabilities('current');
  assert.equal(currentCaps.hasThumb, true);
  assert.equal(currentCaps.hasTags, true);
  assert.equal(currentCaps.hasAudioThumb, true);
  assert.equal(currentCaps.thumbMaxSize, 1024);
  assert.deepEqual(store.missingFeatures('current'), []);

  // Pre-tags machine: older silently (no missingFeatures), thumbMaxSize 512, no tags/audio thumbs.
  const preCaps = store.fileCapabilities('pretags');
  assert.equal(preCaps.hasThumb, true);
  assert.equal(preCaps.hasTags, false);
  assert.equal(preCaps.hasAudioThumb, false);
  assert.equal(preCaps.thumbMaxSize, 512);
  assert.deepEqual(store.missingFeatures('pretags'), []);

  // Legacy 05ba1ef machine without fs.thumb reports 0 / false cleanly.
  const legacyCaps = store.fileCapabilities('legacy');
  assert.equal(legacyCaps.hasThumb, false);
  assert.equal(legacyCaps.hasTags, false);
  assert.equal(legacyCaps.hasAudioThumb, false);
  assert.equal(legacyCaps.thumbMaxSize, 0);

  // Role with fs.thumb but without fs.tags still advertises audio thumbnails and thumbMaxSize 1024.
  const limitedCaps = computeFileCapabilities({
    known: true,
    tools: NEW_TOOLS,
    allowedSet: new Set(['fs.list', 'fs.read', 'fs.thumb']),
  });
  assert.equal(limitedCaps.hasTags, false);
  assert.equal(limitedCaps.hasAudioThumb, true);
  assert.equal(limitedCaps.thumbMaxSize, 1024);

  store.dispose();
});
