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
