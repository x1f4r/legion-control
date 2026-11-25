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

    // state() must pass explicit `on: <selected node>` for meta.whoami and meta.version.
    const whoamiCall = calls.find((c) => c.command === 'call' && c.payload.tool === 'meta.whoami');
    const versionCall = calls.find((c) => c.command === 'call' && c.payload.tool === 'meta.version');
    assert.equal(whoamiCall?.payload?.on, 'tower');
    assert.equal(versionCall?.payload?.on, 'tower');
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink state() keeps selected node unreachable with last known role and never substitutes another node', async () => {
  const prevWindow = globalThis.window;
  const seenCalls = [];
  const checkedAt = '2026-11-24T22:00:00Z';
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          seenCalls.push({ command, payload });
          if (command === 'state') {
            return Promise.resolve({
              ready: true,
              nodes: [
                {
                  node: 'tower',
                  principal: 'device:pixel',
                  role: 'operator',
                  role_checked_at: checkedAt,
                  via: 'ssh',
                },
                {
                  node: 'pi',
                  principal: 'device:pixel',
                  role: 'observer',
                  via: 'ssh',
                },
              ],
            });
          }
          if (command === 'call' && payload.tool === 'meta.whoami') {
            assert.equal(payload.on, 'tower');
            return Promise.reject({
              code: 'unreachable',
              message: 'Could not reach tower.',
              machine: 'tower',
            });
          }
          if (command === 'call' && payload.tool === 'meta.version') {
            assert.equal(payload.on, 'tower');
            return Promise.reject({
              code: 'unreachable',
              message: 'Could not reach tower.',
              machine: 'tower',
            });
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
    const st = await link.state();
    assert.equal(st.ready, true);
    assert.equal(st.error, undefined);
    assert.equal(st.nodes[0].node, 'tower');
    assert.equal(st.nodes[0].reachable, false);
    assert.equal(st.nodes[0].whoamiStale, true);
    assert.equal(st.nodes[0].role, 'operator');
    assert.equal(st.nodes[0].roleCheckedAt, checkedAt);
    assert.equal(st.nodes[0].whoami?.role, 'operator');
    assert.equal(st.nodes[0].whoami?.stale, true);
    assert.equal(st.nodes[1].node, 'pi');
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink wraps native update commands and live update watch channel', async () => {
  const prevWindow = globalThis.window;
  const seen = [];
  let registeredChannel = null;
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          seen.push({ command, payload });
          if (command === 'update_watch') {
            registeredChannel = payload.onState;
            return Promise.resolve();
          }
          if (command === 'update_state' || command === 'update_check') {
            return Promise.resolve({ phase: 'ready', auto_update: true });
          }
          if (command === 'update_set_auto') {
            return Promise.resolve({ phase: 'ready', auto_update: payload.enabled });
          }
          return Promise.resolve({ phase: 'up_to_date', auto_update: true });
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
    const observed = [];
    const unwatch = link.watchUpdates((st) => observed.push(st));
    assert.ok(registeredChannel);
    registeredChannel.onmessage({ phase: 'downloading', progress: { downloaded_bytes: 5, total_bytes: 10 } });
    assert.equal(observed.length, 1);
    assert.equal(observed[0].phase, 'downloading');
    unwatch();
    registeredChannel.onmessage({ phase: 'ready' });
    assert.equal(observed.length, 1);

    assert.equal((await link.updateState()).phase, 'ready');
    assert.equal((await link.checkUpdates()).phase, 'ready');
    assert.equal((await link.setAutoUpdate(false)).auto_update, false);
    await link.installUpdate();
    await link.dismissUpdateNotice();
    await link.maybeAutoInstall('startup');
    await link.maybeAutoInstall('background');
    for (const cmd of [
      'update_watch',
      'update_state',
      'update_check',
      'update_set_auto',
      'update_install',
      'update_dismiss_notice',
      'update_maybe_auto_install',
    ]) {
      assert.ok(seen.some((c) => c.command === cmd), `expected ${cmd}`);
    }
    assert.ok(
      seen.some((c) => c.command === 'update_maybe_auto_install' && c.payload?.trigger === 'startup'),
      'expected startup trigger payload',
    );
    assert.ok(
      seen.some((c) => c.command === 'update_maybe_auto_install' && c.payload?.trigger === 'background'),
      'expected background trigger payload',
    );
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink implements files-contract contentUrl and device transfer queue methods', async () => {
  const prevWindow = globalThis.window;
  const seen = [];
  let transferChannel = null;
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          seen.push({ command, payload });
          if (command === 'content_url') {
            return Promise.resolve({
              url: 'http://127.0.0.1:43210/v1/content/tok/pic.jpg',
              expires_in: 900,
            });
          }
          if (command === 'content_revoke') {
            return Promise.resolve(true);
          }
          if (command === 'transfer_download') {
            return Promise.resolve({ id: 'tr-1' });
          }
          if (command === 'transfer_pick_uploads') {
            return Promise.resolve([
              { handle: 'up-00112233', name: 'note.txt', size: 12, mime: 'text/plain' },
            ]);
          }
          if (command === 'transfer_upload') {
            return Promise.resolve(payload.dir === '/cancelled' ? null : { id: 'tr-2', ids: ['tr-2'], count: 1 });
          }
          if (command === 'transfer_open_externally') {
            return Promise.resolve({ id: 'tr-3', ok: true });
          }
          if (command === 'transfers') {
            return Promise.resolve([
              { id: 'tr-1', direction: 'download', state: 'running', bytes_done: 10, bytes_total: 100 },
            ]);
          }
          if (command === 'transfer_control') {
            return Promise.resolve({ id: payload.id, state: payload.action === 'pause' ? 'paused' : 'cancelled' });
          }
          if (command === 'open_transfer' || command === 'show_transfer_in_folder') {
            return Promise.resolve({ ok: true });
          }
          if (command === 'transfer_watch') {
            transferChannel = payload.onEvent;
            return Promise.resolve(77);
          }
          if (command === 'transfer_unwatch') {
            return Promise.resolve();
          }
          if (command === 'transfer_settings' || command === 'transfer_set_settings') {
            return Promise.resolve({ wifi_only_large: Boolean(payload?.wifiOnlyLarge), large_threshold_bytes: 50000000 });
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

    const urlRes = await link.contentUrl({ machine: 'tower', path: '/home/me/pic.jpg', download: true });
    assert.equal(urlRes.url, 'http://127.0.0.1:43210/v1/content/tok/pic.jpg');
    assert.equal(urlRes.expires_in, 900);
    const urlCall = seen.find((c) => c.command === 'content_url');
    assert.deepEqual(urlCall.payload, { machine: 'tower', path: '/home/me/pic.jpg', download: true });

    const aborted = new AbortController();
    aborted.abort();
    await assert.rejects(() => link.contentUrl({ machine: 'tower', path: '/a', signal: aborted.signal }), {
      code: 'cancelled',
    });

    assert.deepEqual(
      await link.download({ machine: 'tower', path: '/home/me/clip.webm', name: 'clip.webm', size: 100, mime: 'video/webm' }),
      { id: 'tr-1' },
    );
    const picked = await link.pickUploads();
    assert.equal(picked[0].handle, 'up-00112233');
    assert.deepEqual(
      await link.upload({
        machine: 'tower',
        dir: '/home/me/uploads',
        handles: ['up-00112233'],
        files: [{ path: '/etc/passwd' }],
      }),
      {
        id: 'tr-2',
        ids: ['tr-2'],
        count: 1,
      },
    );
    const uploadCall = seen.find((c) => c.command === 'transfer_upload');
    assert.deepEqual(uploadCall.payload, {
      machine: 'tower',
      dir: '/home/me/uploads',
      handles: ['up-00112233'],
    });
    assert.equal('files' in uploadCall.payload, false);
    assert.equal(await link.upload({ machine: 'tower', dir: '/cancelled' }), null);
    assert.deepEqual(
      await link.openExternally({
        machine: 'tower',
        path: '/home/me/doc.pdf',
        size: 4096,
        mime: 'application/pdf',
      }),
      { id: 'tr-3', ok: true },
    );
    const openCall = seen.find((c) => c.command === 'transfer_open_externally');
    assert.deepEqual(openCall.payload, {
      machine: 'tower',
      path: '/home/me/doc.pdf',
      name: null,
      size: 4096,
      mime: 'application/pdf',
    });
    assert.equal(typeof link.contentStatus, 'function');
    assert.equal(typeof link.revokeContentUrl, 'function');
    assert.equal(await link.revokeContentUrl(urlRes.url), true);
    const revokeCall = seen.find((c) => c.command === 'content_revoke');
    assert.deepEqual(revokeCall.payload, { url: urlRes.url });
    assert.equal(await link.revokeContentUrl(''), false);

    const list = await link.transfers();
    assert.equal(list.length, 1);
    assert.equal(list[0].id, 'tr-1');

    const paused = await link.transferControl('tr-1', 'pause');
    assert.equal(paused.state, 'paused');
    assert.deepEqual(await link.openTransfer('tr-1'), { ok: true });
    assert.deepEqual(await link.showTransferInFolder('tr-1'), { ok: true });

    const events = [];
    const unsub = link.onTransferEvent((items) => events.push(items));
    assert.ok(transferChannel);
    await Promise.resolve();
    transferChannel.onmessage([{ id: 'tr-1', state: 'done' }]);
    assert.equal(events.length, 1);
    assert.equal(events[0][0].state, 'done');
    unsub();
    await Promise.resolve();
    assert.ok(seen.some((c) => c.command === 'transfer_unwatch' && c.payload.watcherId === 77));
    transferChannel.onmessage([{ id: 'tr-1', state: 'cancelled' }]);
    assert.equal(events.length, 1);

    const s = await link.setTransferSettings({ wifi_only_large: true });
    assert.equal(s.wifi_only_large, true);
    assert.equal((await link.transferSettings()).large_threshold_bytes, 50000000);
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink hides unsupported upload and openExternally methods on iOS', () => {
  const prevNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15' },
  });
  try {
    const link = createAppLink();
    assert.equal(typeof link.download, 'function');
    assert.equal(typeof link.upload, 'undefined');
    assert.equal(typeof link.pickUploads, 'undefined');
    assert.equal(typeof link.openExternally, 'undefined');
  } finally {
    if (prevNavigator) {
      Object.defineProperty(globalThis, 'navigator', prevNavigator);
    } else {
      delete globalThis.navigator;
    }
  }
});

test('createAppLink advertises content support, remembers unsupported remote machines, and forgets on capability contact', async () => {
  const prevWindow = globalThis.window;
  let legacySupported = false;
  let piSupported = false;
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'state') {
            return Promise.resolve({
              ready: true,
              nodes: [{ node: 'legacy', principal: 'phone', role: 'admin', via: 'ssh' }],
            });
          }
          if (command === 'select_node') return Promise.resolve();
          if (command === 'tools') return Promise.resolve([{ name: 'fs.list' }]);
          if (command === 'call') {
            if (payload.tool === 'fs.list') {
              return Promise.resolve({ entries: [] });
            }
            if (payload.tool === 'meta.version' && payload.on === 'all') {
              return Promise.resolve({
                results: {
                  legacy: { ok: { node: 'legacy', version: '0.2.0' } },
                  pi: { error: { code: 'unreachable', message: 'offline' } },
                },
              });
            }
            if (payload.tool === 'meta.version') {
              return Promise.resolve({ node: payload.on || 'legacy', version: '0.2.0' });
            }
            if (payload.tool === 'meta.whoami') {
              return Promise.resolve({
                node: payload.on || 'legacy',
                principal: 'phone',
                role: 'admin',
                tools: ['fs.list'],
              });
            }
            if (payload.tool === 'meta.tools') {
              return Promise.resolve({ node: payload.on || 'legacy', tools: [] });
            }
            return Promise.resolve({});
          }
          if (command === 'content_url') {
            if (payload.machine === 'legacy' && !legacySupported) {
              return Promise.reject({
                code: 'unsupported',
                message: 'legacy answered stream get without modification time.',
                machine: 'legacy',
              });
            }
            if (payload.machine === 'pi' && !piSupported) {
              return Promise.reject({
                code: 'unsupported',
                message: 'pi answered stream get without modification time.',
                machine: 'pi',
              });
            }
            if (payload.path === '/huge.png') {
              return Promise.reject({
                code: 'unsupported',
                message: 'This file is too large to show here.',
                machine: payload.machine,
                details: { reason: 'too_large_for_inline' },
              });
            }
            if (payload.path === '/offline.png') {
              return Promise.reject({
                code: 'unreachable',
                message: 'Machine is unreachable.',
                machine: payload.machine,
              });
            }
            return Promise.resolve({ url: 'http://127.0.0.1:42000/v1/content/tok/pic.png', expires_in: 900 });
          }
          if (command === 'invalidate_sessions') return Promise.resolve();
          return Promise.resolve({});
        },
      },
    },
  };

  try {
    const link = createAppLink();
    let notifications = 0;
    const unsub = link.onContentSupportChange(() => {
      notifications += 1;
    });
    assert.equal(link.supportsContent, true);
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(link.supportsContentFor('legacy'), true);

    // Per-file size limits and transient errors must not mark a machine unsupported.
    await assert.rejects(() => link.contentUrl({ machine: 'tower', path: '/huge.png' }), {
      code: 'unsupported',
    });
    await assert.rejects(() => link.contentUrl({ machine: 'tower', path: '/offline.png' }), {
      code: 'unreachable',
    });
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(notifications, 0);

    // Machine-level unsupported marks only that machine.
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '/a.png' }), {
      code: 'unsupported',
      machine: 'legacy',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(notifications, 1);

    // Ordinary non-capability calls do not clear the unsupported marker.
    await link.call('fs.list', { path: '~' }, { on: 'legacy' });
    assert.equal(link.supportsContentFor('legacy'), false);
    assert.equal(notifications, 1);

    // Single-machine meta.version forgets unsupported for that machine only.
    await assert.rejects(() => link.contentUrl({ machine: 'pi', path: '/b.png' }), {
      code: 'unsupported',
      machine: 'pi',
    });
    assert.equal(link.supportsContentFor('pi'), false);
    assert.equal(notifications, 2);

    await link.call('meta.version', {}, { on: 'legacy' });
    assert.equal(link.supportsContentFor('legacy'), true);
    assert.equal(link.supportsContentFor('pi'), false);
    assert.equal(notifications, 3);

    // Fan-out capability contact forgets only machines whose entries succeeded (not errors).
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '/a.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    assert.equal(notifications, 4);

    await link.call('meta.version', {}, { on: 'all' });
    assert.equal(link.supportsContentFor('legacy'), true);
    assert.equal(link.supportsContentFor('pi'), false);
    assert.equal(notifications, 5);

    // meta.whoami clears unsupported for that machine.
    await link.call('meta.whoami', {}, { on: 'pi' });
    assert.equal(link.supportsContentFor('pi'), true);
    assert.equal(notifications, 6);

    // meta.tools clears unsupported for that machine.
    await assert.rejects(() => link.contentUrl({ machine: 'pi', path: '/b.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('pi'), false);
    await link.call('meta.tools', {}, { on: 'pi' });
    assert.equal(link.supportsContentFor('pi'), true);

    // tools({ on }) clears unsupported for that machine.
    await assert.rejects(() => link.contentUrl({ machine: 'pi', path: '/b.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('pi'), false);
    await link.tools({ on: 'pi' });
    assert.equal(link.supportsContentFor('pi'), true);

    // state() refreshes meta.whoami/meta.version for the selected node and clears unsupported.
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '/a.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    await link.state();
    assert.equal(link.supportsContentFor('legacy'), true);

    // Default-selected node tools() without explicit `on` also clears the selected node.
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '/a.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    await link.tools();
    assert.equal(link.supportsContentFor('legacy'), true);

    // A successful contentUrl clears an earlier unsupported marker (e.g. after remote upgrade).
    await assert.rejects(() => link.contentUrl({ machine: 'pi', path: '/b.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('pi'), false);
    piSupported = true;
    const minted = await link.contentUrl({ machine: 'pi', path: '/b.png' });
    assert.ok(minted.url);
    assert.equal(link.supportsContentFor('pi'), true);

    // Session invalidation still clears any remaining unsupported markers.
    legacySupported = false;
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '/a.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    const beforeInvalidate = notifications;
    await link.invalidateSessions();
    assert.equal(link.supportsContentFor('legacy'), true);
    assert.equal(notifications, beforeInvalidate + 1);
    unsub();
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});
