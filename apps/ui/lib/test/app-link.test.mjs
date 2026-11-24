import assert from 'node:assert/strict';
import test from 'node:test';

import { createAppLink } from '../../link/app.js';

test('createAppLink uses a distinct random call ID prefix per page load', async () => {
  const calls = [];
  const prevWindow = globalThis.window;
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          return Promise.resolve({ ok: true });
        },
        Channel: class FakeChannel {
          constructor() {
            this.onmessage = null;
          }
        },
      },
    },
  };

  try {
    const first = createAppLink();
    const second = createAppLink();

    await first.call('sys.info', {});
    await first.call('sys.info', {});
    await second.call('sys.info', {});

    assert.equal(calls.length, 3);
    const [id1, id2, id3] = calls.map((c) => c.payload.id);
    assert.match(id1, /^[0-9a-f]{16}-1$/);
    assert.match(id2, /^[0-9a-f]{16}-2$/);
    assert.match(id3, /^[0-9a-f]{16}-1$/);
    assert.equal(id1.slice(0, 16), id2.slice(0, 16));
    assert.notEqual(id1.slice(0, 16), id3.slice(0, 16));
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink uses origin-scoped Android bridge for deviceModel and local network permission', async () => {
  const prevWindow = globalThis.window;
  let requested = 0;
  let invokedDeviceName = 0;
  globalThis.window = {
    __LEGION_ANDROID__: {
      deviceModel: () => 'Pixel 10',
      hasLocalNetwork: () => false,
      requestLocalNetwork: () => {
        requested += 1;
        return Promise.resolve(false);
      },
    },
    __TAURI__: {
      core: {
        invoke(command) {
          if (command === 'device_name') invokedDeviceName += 1;
          return Promise.resolve('fallback');
        },
      },
    },
  };

  try {
    const link = createAppLink();
    assert.equal(await link.deviceName(), 'Pixel 10');
    assert.equal(invokedDeviceName, 0);

    await assert.rejects(() => link.pair('legion-invite-1:abc', 'Pixel 10'), {
      code: 'denied',
    });
    assert.equal(requested, 1);
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink exposes learn, acceptIdentityChange, and preserves last_paired from pair', async () => {
  const calls = [];
  const prevWindow = globalThis.window;
  const lastPaired = {
    node: 'tower',
    device: 'Pixel 10',
    role: 'operator',
    synced: ['pi'],
    not_synced: [],
  };
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'pair') {
            return Promise.resolve({
              ready: true,
              nodes: [{ node: 'tower', principal: 'device:pixel', role: 'operator', via: 'ssh' }],
              last_paired: lastPaired,
            });
          }
          if (command === 'state') {
            return Promise.resolve({
              ready: true,
              nodes: [{ node: 'tower', principal: 'device:pixel', role: 'operator', via: 'ssh' }],
              routes: { tower: { kind: 'direct' } },
            });
          }
          if (command === 'call' && payload.tool === 'meta.whoami') {
            return Promise.resolve({ node: 'tower', principal: 'device:pixel', role: 'operator', tools: [] });
          }
          if (command === 'call' && payload.tool === 'meta.version') {
            return Promise.resolve({ version: '0.1.0', protocol: 1 });
          }
          if (command === 'learn') {
            return Promise.resolve({
              report: { introducer: 'tower', added: ['pi'], updated: [], identity_changed: [], skipped: [] },
              state: { ready: true, nodes: [{ node: 'tower' }, { node: 'pi' }] },
            });
          }
          if (command === 'accept_identity_change') {
            return Promise.resolve({ ready: true, nodes: [{ node: 'tower' }] });
          }
          if (command === 'dismiss_identity_change') {
            return Promise.resolve({ ready: true, nodes: [{ node: 'tower' }] });
          }
          return Promise.resolve({});
        },
        Channel: class FakeChannel {
          constructor() {
            this.onmessage = null;
          }
        },
      },
    },
  };

  try {
    const link = createAppLink();
    const pairedState = await link.pair('legion-invite-1:abc', 'Pixel 10');
    assert.deepEqual(pairedState.last_paired, lastPaired);

    const learned = await link.learn();
    assert.deepEqual(learned.report.added, ['pi']);

    await link.acceptIdentityChange('mini', ['SHA256:abc']);
    assert.ok(
      calls.some(
        (c) =>
          c.command === 'accept_identity_change' &&
          c.payload.node === 'mini' &&
          Array.isArray(c.payload.fingerprints) &&
          c.payload.fingerprints[0] === 'SHA256:abc',
      ),
    );

    await link.dismissIdentityChange('mini');
    assert.ok(calls.some((c) => c.command === 'dismiss_identity_change' && c.payload.node === 'mini'));
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});
