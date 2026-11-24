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
