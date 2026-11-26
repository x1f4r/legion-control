import assert from 'node:assert/strict';
import test from 'node:test';

import { createAppLink } from '../../link/app.js';
import { createForegroundResumeController } from '../resume.js';

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

test('createAppLink routes immersive and keepScreenOn across Android, iOS, and desktop Tauri', async () => {
  const prevWindow = globalThis.window;
  const prevNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');

  try {
    // 1. Android bridge
    const androidImmersive = [];
    const androidKeep = [];
    const androidHaptics = [];
    globalThis.window = {
      __LEGION_ANDROID__: {
        setImmersive: (on, orientation) => {
          androidImmersive.push({ on, orientation });
          return Promise.resolve(on);
        },
        setKeepScreenOn: (on) => {
          androidKeep.push(on);
          return Promise.resolve(Boolean(on));
        },
        haptic: (kind) => {
          androidHaptics.push(kind);
        },
      },
      __TAURI__: { core: { invoke: () => Promise.resolve({}) } },
    };
    const androidLink = createAppLink();
    assert.equal(androidLink.canImmersive(), true);
    assert.equal(typeof androidLink.haptic, 'function');
    assert.deepEqual(await androidLink.immersive(true, { orientation: 'landscape' }), { active: true, mode: 'android' });
    assert.deepEqual(await androidLink.immersive(false), { active: false, mode: 'android' });
    assert.equal(await androidLink.keepScreenOn(true), true);
    assert.equal(await androidLink.keepScreenOn(false), false);
    androidLink.haptic('tick');
    androidLink.haptic('tap');
    androidLink.haptic('unknown');
    assert.deepEqual(androidImmersive, [
      { on: true, orientation: 'landscape' },
      { on: false, orientation: 'none' },
    ]);
    assert.deepEqual(androidKeep, [true, false]);
    assert.deepEqual(androidHaptics, ['tick', 'tap']);

    // 2. iOS bridge
    const iosMessages = [];
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15' },
    });
    globalThis.window = {
      webkit: {
        messageHandlers: {
          legionImmersive: {
            postMessage: (msg) => iosMessages.push(msg),
          },
        },
      },
      __TAURI__: { core: { invoke: () => Promise.resolve({}) } },
    };
    const iosLink = createAppLink();
    assert.equal(iosLink.canImmersive(), true);
    assert.equal(typeof iosLink.haptic, 'function');
    await iosLink.immersive(true, { orientation: 'portrait' });
    await iosLink.keepScreenOn(true);
    iosLink.haptic('tick');
    iosLink.haptic('tap');
    iosLink.haptic('invalid');
    await iosLink.immersive(false);
    assert.deepEqual(iosMessages, [
      { type: 'immersive', on: true, orientation: 'portrait' },
      { type: 'keep_screen_on', on: true },
      { type: 'haptic', kind: 'tick' },
      { type: 'haptic', kind: 'tap' },
      { type: 'immersive', on: false, orientation: 'none' },
    ]);

    // 3. Desktop Tauri commands (haptic is absent on desktop)
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { userAgent: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15' },
    });
    const desktopCalls = [];
    globalThis.window = {
      __TAURI__: {
        core: {
          invoke(command, payload) {
            desktopCalls.push({ command, payload });
            if (command === 'window_set_immersive') {
              return Promise.resolve({ active: payload.on, fullscreen: payload.on });
            }
            if (command === 'set_keep_screen_on') {
              return Promise.resolve(Boolean(payload.on));
            }
            return Promise.resolve();
          },
        },
      },
    };
    const desktopLink = createAppLink();
    assert.equal(desktopLink.canImmersive(), true);
    assert.equal(desktopLink.haptic, undefined);
    assert.deepEqual(await desktopLink.immersive(true, { orientation: 'landscape' }), {
      active: true,
      fullscreen: true,
      mode: 'desktop',
    });
    assert.equal(await desktopLink.keepScreenOn(true), true);
    await desktopLink.immersive(false);
    assert.equal(await desktopLink.keepScreenOn(false), false);
    assert.deepEqual(desktopCalls, [
      { command: 'window_set_immersive', payload: { on: true } },
      { command: 'set_keep_screen_on', payload: { on: true } },
      { command: 'window_set_immersive', payload: { on: false } },
      { command: 'set_keep_screen_on', payload: { on: false } },
    ]);

    // When desktop backend reports false (e.g. Linux without portal or ScreenSaver), keepScreenOn reports false honestly.
    globalThis.window = {
      __TAURI__: {
        core: {
          invoke(command) {
            if (command === 'set_keep_screen_on') return Promise.resolve(false);
            return Promise.resolve();
          },
        },
      },
    };
    const unavailableDesktopLink = createAppLink();
    assert.equal(await unavailableDesktopLink.keepScreenOn(true), false);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevNavigator) Object.defineProperty(globalThis, 'navigator', prevNavigator);
    else delete globalThis.navigator;
  }
});

test('createAppLink does not mark relay or target unsupported on relay_too_old and clears cache when learn adds direct routes', async () => {
  const prevWindow = globalThis.window;
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'content_url') {
            if (payload.machine === 'studio') {
              return Promise.reject({
                code: 'unsupported',
                message: 'tower needs an update before it can pass files along from studio.',
                machine: 'tower',
                details: { reason: 'relay_too_old', relay: 'tower', target: 'studio' },
              });
            }
            return Promise.reject({
              code: 'unsupported',
              message: 'legacy answered stream get without modification time.',
              machine: 'legacy',
            });
          }
          if (command === 'learn') {
            return Promise.resolve({
              report: { introducer: 'tower', added: ['legacy'], updated: [], identity_changed: [], skipped: [] },
              state: { ready: true, nodes: [{ node: 'tower' }, { node: 'legacy' }] },
            });
          }
          return Promise.resolve({});
        },
      },
    },
  };

  try {
    const link = createAppLink();
    await assert.rejects(() => link.contentUrl({ machine: 'studio', path: '/pic.png' }), {
      code: 'unsupported',
    });
    // Neither the old relay nor the target should be falsely marked unsupported for direct content.
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(link.supportsContentFor('studio'), true);

    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '/pic.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);

    // Learning new direct routes clears cached unsupported entries.
    await link.learn();
    assert.equal(link.supportsContentFor('legacy'), true);
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink onAppResume listens to native tauri://resumed and deduplicates focus fallback', async () => {
  const prevWindow = globalThis.window;
  const handlers = new Map();
  let unlistenCount = 0;

  globalThis.window = {
    __TAURI__: {
      core: {
        invoke: () => Promise.resolve({}),
      },
      event: {
        listen(name, cb) {
          handlers.set(name, cb);
          return Promise.resolve(() => {
            unlistenCount += 1;
            handlers.delete(name);
          });
        },
      },
    },
  };

  try {
    const link = createAppLink();
    const events = [];
    const suspends = [];
    const unsub = link.onAppResume((ev) => events.push(ev.source));
    const unsubSuspend = link.onAppSuspend((ev) => suspends.push(ev.source));
    // Allow async listen() promises to settle.
    await new Promise((r) => setTimeout(r, 0));

    assert.ok(handlers.has('tauri://resumed'));
    assert.ok(handlers.has('tauri://suspended'));
    assert.ok(handlers.has('tauri://focus'));
    assert.equal(link.isAppSuspended(), false);

    // Focus alone without prior suspend (e.g. startup or desktop focus) never triggers resume.
    handlers.get('tauri://focus')({});
    assert.deepEqual(events, []);

    // Native tauri://resumed fires even when neither suspended nor hidden was ever seen.
    handlers.get('tauri://resumed')({});
    assert.deepEqual(events, ['tauri://resumed']);

    // Standard iOS/Android sequence: suspended -> resumed -> focus fires once only.
    handlers.get('tauri://suspended')({});
    assert.equal(link.isAppSuspended(), true);
    assert.deepEqual(suspends, ['tauri://suspended']);
    handlers.get('tauri://resumed')({});
    assert.equal(link.isAppSuspended(), false);
    handlers.get('tauri://focus')({});
    assert.deepEqual(events, ['tauri://resumed', 'tauri://resumed']);

    // Short iOS inactive -> active without sceneDidEnterBackground: suspended -> focus fires once.
    handlers.get('tauri://suspended')({});
    assert.equal(link.isAppSuspended(), true);
    handlers.get('tauri://focus')({});
    assert.equal(link.isAppSuspended(), false);
    assert.deepEqual(events, ['tauri://resumed', 'tauri://resumed', 'tauri://focus']);
    assert.deepEqual(suspends, ['tauri://suspended', 'tauri://suspended']);

    unsub();
    assert.equal(unlistenCount, 0);
    unsubSuspend();
    assert.equal(unlistenCount, 3);
    assert.equal(handlers.size, 0);
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('debug media check resume stage resolves from native tauri://resumed even when web view never sees hidden', async () => {
  const prevWindow = globalThis.window;
  const prevDocument = globalThis.document;
  const prevImage = globalThis.Image;

  const tauriListeners = new Map();
  const winListeners = new Map();
  const docListeners = new Map();
  const reports = [];
  const order = [];
  let listenSettled = false;

  function makeEventTarget(map) {
    return {
      addEventListener(name, fn) {
        if (!map.has(name)) map.set(name, new Set());
        map.get(name).add(fn);
      },
      removeEventListener(name, fn) {
        map.get(name)?.delete(fn);
      },
      dispatch(name) {
        for (const fn of [...(map.get(name) || [])]) fn();
      },
    };
  }

  const winTarget = makeEventTarget(winListeners);
  const docTarget = makeEventTarget(docListeners);

  globalThis.Image = class FakeImage {
    constructor() {
      this.naturalWidth = 16;
      this.naturalHeight = 16;
      this.onload = null;
      this.onerror = null;
    }
    set src(_v) {
      queueMicrotask(() => this.onload?.());
    }
    decode() {
      return Promise.resolve();
    }
  };

  globalThis.document = {
    visibilityState: 'visible',
    body: {
      appendChild() {},
    },
    addEventListener: docTarget.addEventListener,
    removeEventListener: docTarget.removeEventListener,
    createElement(tag) {
      assert.equal(tag, 'video');
      const vListeners = new Map();
      const video = {
        muted: false,
        playsInline: false,
        preload: '',
        style: {},
        videoWidth: 64,
        videoHeight: 64,
        duration: 3,
        currentTime: 0,
        readyState: 4,
        networkState: 1,
        setAttribute() {},
        removeAttribute() {},
        pause() {},
        load() {},
        remove() {},
        addEventListener(name, fn) {
          if (!vListeners.has(name)) vListeners.set(name, new Set());
          vListeners.get(name).add(fn);
        },
        removeEventListener(name, fn) {
          vListeners.get(name)?.delete(fn);
        },
        set src(_v) {
          queueMicrotask(() => {
            for (const fn of [...(vListeners.get('loadeddata') || [])]) fn();
          });
        },
      };
      return video;
    },
  };

  let intermediateResolve;
  const intermediateReportSeen = new Promise((r) => {
    intermediateResolve = r;
  });
  let finalResolve;
  const finalReportSeen = new Promise((r) => {
    finalResolve = r;
  });

  globalThis.window = {
    location: { origin: 'tauri://localhost' },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    addEventListener: winTarget.addEventListener,
    removeEventListener: winTarget.removeEventListener,
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'state') {
            return Promise.resolve({ ready: true, nodes: [{ node: 'tower' }] });
          }
          if (command === 'ready') return Promise.resolve();
          if (command === 'media_check_spec') {
            return Promise.resolve({
              machine: 'tower',
              image_path: '/home/me/a.png',
              video_path: '/home/me/b.mp4',
              verify_resume: true,
            });
          }
          if (command === 'content_url') {
            return Promise.resolve({
              url: `http://127.0.0.1:49999/v1/content/tok/${payload.path.split('/').pop()}`,
              expires_in: 900,
            });
          }
          if (command === 'media_check_report') {
            reports.push(payload.report);
            order.push(`report:resume_ok=${payload.report.resume_ok}:listenSettled=${listenSettled}`);
            if (payload.report.resume_ok === false) intermediateResolve(payload.report);
            if (payload.report.resume_ok === true) finalResolve(payload.report);
            return Promise.resolve();
          }
          return Promise.resolve({});
        },
      },
      event: {
        listen(name, cb) {
          return new Promise((resolve) => {
            setTimeout(() => {
              tauriListeners.set(name, cb);
              if (name === 'tauri://resumed') {
                listenSettled = true;
                order.push('listen:tauri://resumed');
              }
              resolve(() => tauriListeners.delete(name));
            }, 5);
          });
        },
      },
    },
  };

  try {
    const link = createAppLink();
    await link.state();
    await intermediateReportSeen;

    // Native tauri://resumed listener must have finished registering BEFORE the intermediate report.
    assert.equal(order[0], 'listen:tauri://resumed');
    assert.equal(order[1], 'report:resume_ok=false:listenSettled=true');
    assert.equal(globalThis.document.visibilityState, 'visible');

    // Emit native resume while document.visibilityState stays 'visible' (never hidden).
    tauriListeners.get('tauri://resumed')({});
    const finalReport = await finalReportSeen;

    assert.equal(finalReport.ok, true);
    assert.equal(finalReport.resume_ok, true);
    assert.equal(finalReport.resume_trigger, 'tauri://resumed');
    assert.equal(reports.length, 2);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevDocument === undefined) delete globalThis.document;
    else globalThis.document = prevDocument;
    if (prevImage === undefined) delete globalThis.Image;
    else globalThis.Image = prevImage;
  }
});

test('debug media check resume stage still falls back to DOM visibilitychange when native events are absent', async () => {
  const prevWindow = globalThis.window;
  const prevDocument = globalThis.document;
  const prevImage = globalThis.Image;

  const docListeners = new Map();
  let intermediateResolve;
  const intermediateReportSeen = new Promise((r) => {
    intermediateResolve = r;
  });
  let finalResolve;
  const finalReportSeen = new Promise((r) => {
    finalResolve = r;
  });

  globalThis.Image = class FakeImage {
    constructor() {
      this.naturalWidth = 16;
      this.naturalHeight = 16;
      this.onload = null;
    }
    set src(_v) {
      queueMicrotask(() => this.onload?.());
    }
    decode() {
      return Promise.resolve();
    }
  };

  globalThis.document = {
    visibilityState: 'visible',
    body: { appendChild() {} },
    addEventListener(name, fn) {
      if (!docListeners.has(name)) docListeners.set(name, new Set());
      docListeners.get(name).add(fn);
    },
    removeEventListener(name, fn) {
      docListeners.get(name)?.delete(fn);
    },
    createElement() {
      const vListeners = new Map();
      return {
        style: {},
        videoWidth: 64,
        videoHeight: 64,
        duration: 3,
        currentTime: 0,
        setAttribute() {},
        removeAttribute() {},
        pause() {},
        load() {},
        remove() {},
        addEventListener(name, fn) {
          if (!vListeners.has(name)) vListeners.set(name, new Set());
          vListeners.get(name).add(fn);
        },
        removeEventListener(name, fn) {
          vListeners.get(name)?.delete(fn);
        },
        set src(_v) {
          queueMicrotask(() => {
            for (const fn of [...(vListeners.get('loadeddata') || [])]) fn();
          });
        },
      };
    },
  };

  globalThis.window = {
    location: { origin: 'tauri://localhost' },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    addEventListener() {},
    removeEventListener() {},
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'state') return Promise.resolve({ ready: true, nodes: [{ node: 'tower' }] });
          if (command === 'ready') return Promise.resolve();
          if (command === 'media_check_spec') {
            return Promise.resolve({
              machine: 'tower',
              image_path: '/home/me/a.png',
              video_path: '/home/me/b.mp4',
              verify_resume: true,
            });
          }
          if (command === 'content_url') {
            return Promise.resolve({
              url: `http://127.0.0.1:49999/v1/content/tok/${payload.path.split('/').pop()}`,
              expires_in: 900,
            });
          }
          if (command === 'media_check_report') {
            if (payload.report.resume_ok === false) intermediateResolve(payload.report);
            if (payload.report.resume_ok === true) finalResolve(payload.report);
            return Promise.resolve();
          }
          return Promise.resolve({});
        },
      },
    },
  };

  try {
    const link = createAppLink();
    await link.state();
    await intermediateReportSeen;

    globalThis.document.visibilityState = 'hidden';
    for (const fn of [...(docListeners.get('visibilitychange') || [])]) fn();
    globalThis.document.visibilityState = 'visible';
    for (const fn of [...(docListeners.get('visibilitychange') || [])]) fn();

    const finalReport = await finalReportSeen;
    assert.equal(finalReport.ok, true);
    assert.equal(finalReport.resume_ok, true);
    assert.equal(finalReport.resume_trigger, 'dom-visibility');
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevDocument === undefined) delete globalThis.document;
    else globalThis.document = prevDocument;
    if (prevImage === undefined) delete globalThis.Image;
    else globalThis.Image = prevImage;
  }
});

test('createForegroundResumeController runs session invalidation on native resume without hidden and deduplicates DOM visibility', async () => {
  let visibility = 'visible';
  let clock = 10_000;
  let invalidations = 0;
  let capsInvalidated = 0;
  let refreshes = 0;
  let updateChecks = 0;
  let backgrounds = 0;

  const ctrl = createForegroundResumeController({
    getVisibilityState: () => visibility,
    invalidateSessions: async () => {
      invalidations += 1;
    },
    onBackground: () => {
      backgrounds += 1;
    },
    onInvalidate: () => {
      capsInvalidated += 1;
    },
    onRefresh: () => {
      refreshes += 1;
    },
    onCheckUpdates: () => {
      updateChecks += 1;
    },
    dedupeMs: 1500,
    now: () => clock,
  });

  // 1) Native resume fires even though visibilityState never became 'hidden'.
  assert.equal(await ctrl.handleNativeResume(), true);
  assert.equal(invalidations, 1);
  assert.equal(capsInvalidated, 1);
  assert.equal(refreshes, 1);
  assert.equal(updateChecks, 1);

  // 2) DOM visibilitychange arriving 100ms later for the same foreground return is deduplicated.
  clock += 100;
  visibility = 'hidden';
  await ctrl.handleVisibilityChange();
  assert.equal(backgrounds, 1);
  visibility = 'visible';
  assert.equal(await ctrl.handleVisibilityChange(), false);
  assert.equal(invalidations, 1);
  assert.equal(refreshes, 1);

  // 3) After the dedupe window passes, DOM hidden -> visible runs full recovery as second trigger.
  clock += 2000;
  visibility = 'hidden';
  await ctrl.handleVisibilityChange();
  visibility = 'visible';
  assert.equal(await ctrl.handleVisibilityChange(), true);
  assert.equal(invalidations, 2);
  assert.equal(capsInvalidated, 2);
  assert.equal(refreshes, 2);
  assert.equal(updateChecks, 2);

  // 4) While resume recovery is mid-flight re-establishing sessions, background auto-install hook
  // must NOT fire and onResumingChange reflects the active window accurately.
  clock += 2000;
  let releaseInvalidate;
  let bgWhileResuming = 0;
  const resumingStates = [];
  const slowCtrl = createForegroundResumeController({
    getVisibilityState: () => visibility,
    invalidateSessions: () =>
      new Promise((resolve) => {
        releaseInvalidate = resolve;
      }),
    onResumingChange: (active) => resumingStates.push(active),
    onBackground: () => {
      bgWhileResuming += 1;
    },
    dedupeMs: 1500,
    now: () => clock,
  });

  const resumePromise = slowCtrl.handleNativeResume();
  assert.equal(slowCtrl.isResuming(), true);
  assert.deepEqual(resumingStates, [true]);

  visibility = 'hidden';
  await slowCtrl.handleVisibilityChange();
  assert.equal(bgWhileResuming, 0, 'background install hook must be suppressed during active resume recovery');
  assert.equal(slowCtrl.isHidden(), true);

  releaseInvalidate();
  await resumePromise;
  assert.equal(slowCtrl.isResuming(), false);
  assert.deepEqual(resumingStates, [true, false]);
});

test('createAppLink onImmersiveChange and debug verifyDesktopImmersive cover both WM full-screen sequences', async () => {
  const prevWindow = globalThis.window;
  const prevDocument = globalThis.document;
  const prevImage = globalThis.Image;
  const prevNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');

  const tauriListeners = new Map();
  let realFullscreen = false;
  let immersiveActive = false;
  let enteredByLegion = false;
  let finalReport = null;
  let resolveReport;
  const reportSeen = new Promise((r) => {
    resolveReport = r;
  });

  globalThis.Image = class FakeImage {
    constructor() {
      this.naturalWidth = 16;
      this.naturalHeight = 16;
      this.onload = null;
    }
    set src(_v) {
      queueMicrotask(() => this.onload?.());
    }
    decode() {
      return Promise.resolve();
    }
  };

  globalThis.document = {
    visibilityState: 'visible',
    body: { appendChild() {} },
    addEventListener() {},
    removeEventListener() {},
    createElement() {
      const vListeners = new Map();
      return {
        style: {},
        videoWidth: 64,
        videoHeight: 64,
        duration: 3,
        currentTime: 0,
        setAttribute() {},
        removeAttribute() {},
        pause() {},
        load() {},
        remove() {},
        addEventListener(name, fn) {
          if (!vListeners.has(name)) vListeners.set(name, new Set());
          vListeners.get(name).add(fn);
        },
        removeEventListener(name, fn) {
          vListeners.get(name)?.delete(fn);
        },
        set src(_v) {
          queueMicrotask(() => {
            for (const fn of [...(vListeners.get('loadeddata') || [])]) fn();
          });
        },
      };
    },
  };

  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { userAgent: 'Mozilla/5.0 (X11; Linux x86_64)', platform: 'Linux x86_64', maxTouchPoints: 0 },
  });

  globalThis.window = {
    location: { origin: 'tauri://localhost' },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    addEventListener() {},
    removeEventListener() {},
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'state') return Promise.resolve({ ready: true, nodes: [{ node: 'tower' }] });
          if (command === 'ready') return Promise.resolve();
          if (command === 'media_check_spec') {
            return Promise.resolve({
              machine: 'tower',
              image_path: '/home/me/a.png',
              video_path: '/home/me/b.mp4',
              verify_immersive: true,
            });
          }
          if (command === 'content_url') {
            return Promise.resolve({
              url: `http://127.0.0.1:49999/v1/content/tok/${payload.path.split('/').pop()}`,
              expires_in: 900,
            });
          }
          if (command === 'window_is_fullscreen') {
            return Promise.resolve(realFullscreen);
          }
          if (command === 'window_set_immersive') {
            if (payload.on) {
              if (!immersiveActive) {
                immersiveActive = true;
                enteredByLegion = !realFullscreen;
              }
              if (enteredByLegion) realFullscreen = true;
              return Promise.resolve({ active: true, fullscreen: realFullscreen });
            }
            const shouldExit = immersiveActive && enteredByLegion;
            immersiveActive = false;
            enteredByLegion = false;
            if (shouldExit) realFullscreen = false;
            return Promise.resolve({ active: false, fullscreen: realFullscreen });
          }
          if (command === 'debug_set_window_fullscreen') {
            // Simulates real WM toggle (GTK window.fullscreen()/unfullscreen() firing window-state-event).
            realFullscreen = Boolean(payload.fullscreen);
            if (!realFullscreen && immersiveActive) {
              immersiveActive = false;
              enteredByLegion = false;
              const cb = tauriListeners.get('legion://immersive-changed');
              if (cb) queueMicrotask(() => cb({ payload: false }));
            }
            return Promise.resolve(realFullscreen);
          }
          if (command === 'media_check_report') {
            finalReport = payload.report;
            resolveReport(payload.report);
            return Promise.resolve();
          }
          return Promise.resolve({});
        },
      },
      event: {
        listen(name, cb) {
          tauriListeners.set(name, cb);
          return Promise.resolve(() => tauriListeners.delete(name));
        },
      },
    },
  };

  try {
    const link = createAppLink();
    await link.state();
    await reportSeen;

    assert.equal(finalReport.ok, true);
    assert.equal(finalReport.immersive_check.ok, true);
    assert.equal(finalReport.immersive_check.preFullscreen, true);
    assert.equal(finalReport.immersive_check.afterPreFullscreen, true);
    assert.equal(finalReport.immersive_check.duringSeqB, true);
    assert.equal(finalReport.immersive_check.afterWmExit, false);
    assert.equal(finalReport.immersive_check.wmExitNotified, true);
    // Unsubscribe cleaned up the native listener.
    assert.equal(tauriListeners.has('legion://immersive-changed'), false);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevDocument === undefined) delete globalThis.document;
    else globalThis.document = prevDocument;
    if (prevImage === undefined) delete globalThis.Image;
    else globalThis.Image = prevImage;
    if (prevNavigator) Object.defineProperty(globalThis, 'navigator', prevNavigator);
    else delete globalThis.navigator;
  }
});

test('createAppLink onWindowHidden listens to legion://window-hidden, ends immersive, and cleans up', async () => {
  const prevWindow = globalThis.window;
  const prevNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  const tauriListeners = new Map();
  let unlistenCount = 0;

  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { userAgent: 'Mozilla/5.0 (X11; Linux x86_64)', platform: 'Linux x86_64', maxTouchPoints: 0 },
  });

  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'window_set_immersive') {
            return Promise.resolve({ active: payload.on, fullscreen: payload.on });
          }
          return Promise.resolve({});
        },
      },
      event: {
        listen(name, cb) {
          tauriListeners.set(name, cb);
          return Promise.resolve(() => {
            unlistenCount += 1;
            tauriListeners.delete(name);
          });
        },
      },
    },
  };

  try {
    const link = createAppLink();
    const hiddenEvents = [];
    const immersiveEvents = [];
    const unsubHidden = link.onWindowHidden(() => hiddenEvents.push('hidden'));
    const unsubImm = link.onImmersiveChange((active) => immersiveEvents.push(active));
    await new Promise((r) => setTimeout(r, 0));

    assert.ok(tauriListeners.has('legion://window-hidden'));
    await link.immersive(true);
    tauriListeners.get('legion://window-hidden')({});
    assert.deepEqual(hiddenEvents, ['hidden']);
    assert.deepEqual(immersiveEvents, [false]);

    unsubHidden();
    unsubImm();
    assert.equal(unlistenCount, 2);
    assert.equal(tauriListeners.size, 0);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevNavigator) Object.defineProperty(globalThis, 'navigator', prevNavigator);
    else delete globalThis.navigator;
  }
});

test('createAppLink resolves local pipe to the node name, routes calls through local pipe, and never exposes local', async () => {
  const prevWindow = globalThis.window;
  const calls = [];
  let transferChannel = null;
  let ticketSeq = 0;
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'state') {
            return Promise.resolve({
              ready: true,
              nodes: [
                { node: 'local', principal: 'local', role: 'owner', via: 'local' },
                { node: 'studio', principal: 'device:desktop', role: 'owner', via: 'ssh' },
                { node: 'tower', principal: 'device:desktop', role: 'owner', via: 'ssh' },
              ],
              routes: {
                local: { kind: 'direct' },
                tower: { kind: 'direct' },
              },
            });
          }
          if (command === 'call' && payload.tool === 'meta.whoami') {
            assert.equal(payload.on, 'local');
            return Promise.resolve({ node: 'studio', principal: 'local', role: 'owner', tools: ['fleet.list', 'fs.list'] });
          }
          if (command === 'call' && payload.tool === 'meta.version') {
            assert.equal(payload.on, 'local');
            return Promise.resolve({ version: '0.2.0', protocol: 1 });
          }
          if (command === 'call' && payload.tool === 'fleet.list') {
            return Promise.resolve({
              node: 'studio',
              answered_by: 'local',
              machines: [
                { name: 'studio', this_node: true, systems: [], wake_via: [] },
                { name: 'tower', this_node: false, systems: [], wake_via: [] },
              ],
            });
          }
          if (command === 'call' && payload.tool === 'fs.list') {
            return Promise.resolve({ path: '/home/me', entries: [] });
          }
          if (command === 'tools') {
            return Promise.resolve([{ name: 'fs.list', effect: 'read' }]);
          }
          if (command === 'content_url') {
            ticketSeq += 1;
            const fileName = String(payload.path || 'file').split('/').pop();
            return Promise.resolve({
              url: `http://127.0.0.1:42001/v1/content/tok-${payload.machine}-${ticketSeq}/${fileName}`,
              expires_in: 900,
            });
          }
          if (command === 'content_status') {
            return Promise.resolve({ request_count: 1, status: 200, stream_failed: false, ticket_live: true });
          }
          if (command === 'content_revoke') {
            return Promise.resolve(true);
          }
          if (command === 'transfer_download' || command === 'transfer_upload' || command === 'transfer_open_externally') {
            return Promise.resolve({ id: 'tr-local' });
          }
          if (command === 'transfers') {
            return Promise.resolve([{ id: 'tr-local', machine: 'local', direction: 'download', state: 'running' }]);
          }
          if (command === 'transfer_watch') {
            transferChannel = payload.onEvent;
            return Promise.resolve(99);
          }
          if (command === 'learn') {
            return Promise.resolve({
              report: { introducer: 'local', added: [], updated: [], identity_changed: [], skipped: [] },
              state: {
                ready: true,
                nodes: [
                  { node: 'local', principal: 'local', role: 'owner', via: 'local' },
                  { node: 'studio', principal: 'device:desktop', role: 'owner', via: 'ssh' },
                  { node: 'tower', principal: 'device:desktop', role: 'owner', via: 'ssh' },
                ],
              },
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
    assert.deepEqual(
      st.nodes.map((n) => n.node),
      ['studio', 'tower'],
      'duplicate studio/local nodes must collapse to one local-pipe entry',
    );
    assert.equal(st.nodes[0].via, 'local');
    assert.equal(st.nodes[0].isLocalComputer, true);
    assert.equal(st.nodes[0].nodeNamePending, false);
    assert.equal('local' in st.routes, false);
    assert.ok('studio' in st.routes);

    // Fleet list rewrites answered_by: 'local' to the real node name.
    const fl = await link.call('fleet.list', {});
    assert.equal(fl.answered_by, 'studio');

    // Outgoing calls addressed to the local computer's real node name use the local pipe.
    await link.call('fs.list', { path: '~' }, { on: 'studio' });
    await link.tools({ on: 'studio' });
    const localPic = await link.contentUrl({ machine: 'studio', path: '/home/me/a.png' });
    const localVid = await link.contentUrl({ machine: 'studio', path: '/home/me/clip.mp4' });
    const remotePic = await link.contentUrl({ machine: 'tower', path: '/home/me/b.png' });
    const remoteVid = await link.contentUrl({ machine: 'tower', path: '/home/me/clip.mp4' });
    await link.download({ machine: 'studio', path: '/home/me/a.png' });
    await link.upload({ machine: 'studio', dir: '/home/me' });
    await link.openExternally({ machine: 'studio', path: '/home/me/a.png' });

    assert.equal(calls.find((c) => c.command === 'call' && c.payload.tool === 'fs.list')?.payload?.on, 'local');
    assert.equal(calls.find((c) => c.command === 'tools')?.payload?.on, 'local');
    const urlCalls = calls.filter((c) => c.command === 'content_url');
    assert.deepEqual(
      urlCalls.map((c) => [c.payload.machine, c.payload.path]),
      [
        ['local', '/home/me/a.png'],
        ['local', '/home/me/clip.mp4'],
        ['tower', '/home/me/b.png'],
        ['tower', '/home/me/clip.mp4'],
      ],
    );
    assert.equal(calls.find((c) => c.command === 'transfer_download')?.payload?.machine, 'local');
    assert.equal(calls.find((c) => c.command === 'transfer_upload')?.payload?.machine, 'local');
    assert.equal(calls.find((c) => c.command === 'transfer_open_externally')?.payload?.machine, 'local');

    // Content status and revocation work identically for local-pipe and remote-machine URLs.
    for (const minted of [localPic, localVid, remotePic, remoteVid]) {
      const status = await link.contentStatus(minted.url);
      assert.equal(status.status, 200);
      assert.equal(await link.revokeContentUrl(minted.url), true);
    }
    const revokedUrls = calls.filter((c) => c.command === 'content_revoke').map((c) => c.payload.url);
    assert.deepEqual(revokedUrls, [localPic.url, localVid.url, remotePic.url, remoteVid.url]);
    // Remote machines still keep their own node name.
    await link.call('fs.list', { path: '~' }, { on: 'tower' });
    assert.equal(
      calls.filter((c) => c.command === 'call' && c.payload.tool === 'fs.list').at(-1)?.payload?.on,
      'tower',
    );

    // Transfers and learn() normalize 'local' back to 'studio'.
    const trList = await link.transfers();
    assert.equal(trList[0].machine, 'studio');
    const seenEvents = [];
    const unsub = link.onTransferEvent((items) => seenEvents.push(items));
    transferChannel.onmessage([{ id: 'tr-local', machine: 'local', state: 'done' }]);
    assert.equal(seenEvents[0][0].machine, 'studio');
    unsub();

    const learned = await link.learn();
    assert.equal(learned.report.introducer, 'studio');
    assert.deepEqual(
      learned.state.nodes.map((n) => n.node),
      ['studio', 'tower'],
    );
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink exposes This computer while local node name cannot be read yet and routes through local pipe', async () => {
  const prevWindow = globalThis.window;
  let whoamiReady = false;
  const calls = [];
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'state') {
            return Promise.resolve({
              ready: true,
              nodes: [{ node: 'local', principal: 'local', role: 'owner', via: 'local' }],
            });
          }
          if (command === 'call' && payload.tool === 'meta.whoami') {
            if (!whoamiReady) {
              return Promise.reject({ code: 'unreachable', message: 'Starting up.', machine: 'local' });
            }
            return Promise.resolve({ node: 'studio', principal: 'local', role: 'owner', tools: [] });
          }
          if (command === 'call' && payload.tool === 'meta.version') {
            return Promise.reject({ code: 'unreachable', message: 'Starting up.', machine: 'local' });
          }
          if (command === 'content_url') {
            return Promise.resolve({
              url: 'http://127.0.0.1:42001/v1/content/tok-pending/pic.png',
              expires_in: 900,
            });
          }
          if (command === 'content_revoke') {
            return Promise.resolve(true);
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
    const initial = await link.state();
    assert.equal(initial.nodes.length, 1);
    assert.equal(initial.nodes[0].node, 'This computer');
    assert.equal(initial.nodes[0].isLocalComputer, true);
    assert.equal(initial.nodes[0].nodeNamePending, true);
    assert.equal(initial.nodes[0].unreachableError.machine, 'This computer');

    // Minting and revoking content URLs for "This computer" before name resolution routes over local pipe.
    const pendingUrl = await link.contentUrl({ machine: 'This computer', path: '/home/me/pic.png' });
    assert.equal(calls.find((c) => c.command === 'content_url')?.payload?.machine, 'local');
    assert.equal(await link.revokeContentUrl(pendingUrl.url), true);

    // Subsequent call addressed to "This computer" routes over the local pipe and settles the name.
    whoamiReady = true;
    const who = await link.call('meta.whoami', {}, { on: 'This computer' });
    assert.equal(who.node, 'studio');
    assert.equal(calls.at(-1).payload.on, 'local');
    assert.equal(link.getLocalMachineName(), 'studio');
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink keeps local desktop machine direct, rewrites local relay hops, and strips self learn rejections', async () => {
  const prevWindow = globalThis.window;
  const calls = [];
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'state') {
            return Promise.resolve({
              ready: true,
              // Even when `tower` is active and a duplicate SSH `studio` precedes `local`,
              // the local pipe owns `studio`, stays direct, and rewrites all `local` hops.
              nodes: [
                {
                  node: 'tower',
                  principal: 'device:desktop',
                  role: 'owner',
                  via: 'ssh',
                  origin: 'paired',
                  route: 'direct',
                  route_via: null,
                  learned_from: null,
                },
                {
                  node: 'studio',
                  principal: 'device:desktop',
                  role: 'owner',
                  via: 'ssh',
                  origin: 'learned',
                  route: 'relay',
                  route_via: 'tower',
                  learned_from: 'tower',
                },
                {
                  node: 'local',
                  principal: 'local',
                  role: 'owner',
                  via: 'local',
                  origin: 'paired',
                  route: 'direct',
                  route_via: null,
                },
              ],
              routes: {
                local: { kind: 'direct', via: null, introducer: null },
                studio: { kind: 'relay', via: 'tower', introducer: null },
                tower: { kind: 'direct', via: null, introducer: null },
                pi: { kind: 'relay', via: 'local', introducer: 'local' },
              },
              learn_rejections: [
                {
                  machine: 'studio',
                  code: 'loopback_host',
                  reason: 'Loopback address ignored.',
                  reported_by: 'local',
                },
                {
                  machine: 'nas',
                  code: 'name_mismatch',
                  reason: 'nas answered as nas-backup.',
                  reported_by: 'local',
                },
              ],
              disagreements: [
                {
                  machine: 'studio',
                  reports: [{ reported_by: 'tower', hosts: ['studio.example.net'], port: 22, user: 'me', fingerprints: [] }],
                },
                {
                  machine: 'nas',
                  reports: [
                    { reported_by: 'local', hosts: ['nas-a.example.net'], port: 22, user: 'me', fingerprints: [] },
                    { reported_by: 'tower', hosts: ['nas-b.example.net'], port: 22, user: 'me', fingerprints: [] },
                  ],
                },
              ],
            });
          }
          if (command === 'call' && payload.tool === 'meta.whoami') {
            if (payload.on === 'local') {
              return Promise.resolve({ node: 'studio', principal: 'local', role: 'owner', tools: [] });
            }
            return Promise.resolve({ node: 'tower', principal: 'device:desktop', role: 'owner', tools: [] });
          }
          if (command === 'call' && payload.tool === 'meta.version') {
            return Promise.resolve({ version: '0.2.0', protocol: 1 });
          }
          if (command === 'content_url') {
            return Promise.resolve({ url: 'http://127.0.0.1:42001/v1/content/tok/a.png', expires_in: 900 });
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
    // Resolve local node name first (as happens when `meta.whoami` or `fleet.list` runs on local).
    await link.state();
    await link.call('meta.whoami', {}, { on: 'local' });
    const st = await link.state();

    assert.deepEqual(
      st.nodes.map((n) => n.node),
      ['tower', 'studio'],
    );
    const localNode = st.nodes.find((n) => n.node === 'studio');
    assert.equal(localNode.via, 'local');
    assert.equal(localNode.isLocalComputer, true);
    assert.equal(localNode.route, 'direct');
    assert.equal(localNode.route_via, null);

    // Local computer is ALWAYS direct in `routes`; peer relayed through local names `studio`, not `local`.
    assert.equal('local' in st.routes, false);
    assert.deepEqual(st.routes.studio, { kind: 'direct', via: null, introducer: null });
    assert.equal(st.routes.pi.via, 'studio');
    assert.equal(st.routes.pi.introducer, 'studio');

    // Learn rejections and disagreements never appear for this computer itself, and rewrite `local`.
    assert.deepEqual(
      st.learn_rejections.map((r) => [r.machine, r.reported_by]),
      [['nas', 'studio']],
    );
    assert.deepEqual(
      st.disagreements.map((d) => [d.machine, d.reports.map((r) => r.reported_by)]),
      [['nas', ['studio', 'tower']]],
    );

    // Content URLs for the local computer route over the local pipe.
    await link.contentUrl({ machine: 'studio', path: '/home/me/a.png' });
    assert.equal(calls.find((c) => c.command === 'content_url')?.payload?.machine, 'local');
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('createAppLink pair() forwards AbortSignal to cancel_pair and rejects when already aborted', async () => {
  const prevWindow = globalThis.window;
  const commands = [];
  let rejectPair;

  globalThis.window = {
    __TAURI__: {
      core: {
        invoke(command) {
          commands.push(command);
          if (command === 'pair') {
            return new Promise((_resolve, reject) => {
              rejectPair = reject;
            });
          }
          if (command === 'cancel_pair') {
            rejectPair?.({ code: 'cancelled', message: 'Pairing with tower was cancelled.' });
            return Promise.resolve({});
          }
          return Promise.resolve({});
        },
      },
    },
  };

  try {
    const link = createAppLink();

    // Already-aborted signal rejects before invoking `pair`.
    const preAborted = new AbortController();
    preAborted.abort();
    await assert.rejects(
      () => link.pair('legion-invite-1.abc', 'phone', { signal: preAborted.signal }),
      { code: 'cancelled' },
    );
    assert.deepEqual(commands, []);

    // Aborting mid-flight invokes `cancel_pair` and surfaces `cancelled`.
    const controller = new AbortController();
    const pending = link.pair('legion-invite-1.abc', 'phone', { signal: controller.signal });
    await Promise.resolve();
    controller.abort();
    await assert.rejects(() => pending, { code: 'cancelled' });
    assert.deepEqual(commands, ['pair', 'cancel_pair']);
  } finally {
    if (prevWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = prevWindow;
    }
  }
});

test('debug media check reports separate pairing and media stages and tags pairing failures cleanly', async () => {
  const prevWindow = globalThis.window;
  const prevDocument = globalThis.document;
  const prevImage = globalThis.Image;

  const reports = [];
  let pairDoneResolve;
  const pairDone = new Promise((r) => {
    pairDoneResolve = r;
  });
  let finalResolve;
  const finalSeen = new Promise((r) => {
    finalResolve = r;
  });
  let paired = false;

  globalThis.Image = class FakeImage {
    constructor() {
      this.naturalWidth = 16;
      this.naturalHeight = 16;
      this.onload = null;
    }
    set src(_v) {
      queueMicrotask(() => this.onload?.());
    }
    decode() {
      return Promise.resolve();
    }
  };

  globalThis.document = {
    visibilityState: 'visible',
    body: { appendChild() {} },
    addEventListener() {},
    removeEventListener() {},
    createElement() {
      const vListeners = new Map();
      return {
        style: {},
        videoWidth: 64,
        videoHeight: 64,
        duration: 3,
        currentTime: 0,
        setAttribute() {},
        removeAttribute() {},
        pause() {},
        load() {},
        remove() {},
        addEventListener(name, fn) {
          if (!vListeners.has(name)) vListeners.set(name, new Set());
          vListeners.get(name).add(fn);
        },
        removeEventListener(name, fn) {
          vListeners.get(name)?.delete(fn);
        },
        set src(_v) {
          queueMicrotask(() => {
            for (const fn of [...(vListeners.get('loadeddata') || [])]) fn();
          });
        },
      };
    },
  };

  globalThis.window = {
    location: { origin: 'tauri://localhost' },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    addEventListener() {},
    removeEventListener() {},
    __TAURI__: {
      core: {
        invoke(command, payload) {
          if (command === 'state') {
            return Promise.resolve(
              paired
                ? { ready: true, nodes: [{ node: 'tower' }] }
                : { ready: false, nodes: [], needs: 'pairing' },
            );
          }
          if (command === 'ready') return Promise.resolve();
          if (command === 'media_check_spec') {
            return Promise.resolve({
              machine: 'tower',
              invite: 'legion-invite-1.fake',
              image_path: '/home/me/a.png',
              video_path: '/home/me/b.mp4',
              verify_resume: false,
            });
          }
          if (command === 'pair') {
            paired = true;
            return Promise.resolve({ ready: true, nodes: [{ node: 'tower' }] });
          }
          if (command === 'call' && payload.tool === 'meta.whoami') {
            return Promise.resolve({ node: 'tower', principal: 'device:ci-media', role: 'operator', tools: [] });
          }
          if (command === 'call' && payload.tool === 'meta.version') {
            return Promise.resolve({ version: '0.1.0', protocol: 1 });
          }
          if (command === 'content_url') {
            return Promise.resolve({
              url: `http://127.0.0.1:49999/v1/content/tok/${payload.path.split('/').pop()}`,
              expires_in: 900,
            });
          }
          if (command === 'media_check_report') {
            reports.push(payload.report);
            if (payload.report.stage === 'media' && payload.report.pair_ok === true) {
              pairDoneResolve(payload.report);
            }
            if (payload.report.stage === 'done' || payload.report.error) {
              finalResolve(payload.report);
            }
            return Promise.resolve();
          }
          return Promise.resolve({});
        },
      },
    },
  };

  try {
    const link = createAppLink();
    await link.state();
    await pairDone;
    const finalReport = await finalSeen;

    assert.equal(reports.length, 3);
    assert.equal(reports[0].stage, 'pairing');
    assert.equal(reports[0].pair_ok, false);
    assert.equal(reports[0].error, null);
    assert.equal(reports[1].stage, 'media');
    assert.equal(reports[1].pair_ok, true);
    assert.equal(reports[1].ok, false);
    assert.equal(finalReport.stage, 'done');
    assert.equal(finalReport.pair_ok, true);
    assert.equal(finalReport.ok, true);

    // Pairing failure must report stage='pairing' and pair_ok=false.
    const failReports = [];
    let failResolve;
    const failSeen = new Promise((r) => {
      failResolve = r;
    });
    paired = false;
    globalThis.window.__TAURI__.core.invoke = (command, payload) => {
      if (command === 'state') return Promise.resolve({ ready: false, nodes: [], needs: 'pairing' });
      if (command === 'ready') return Promise.resolve();
      if (command === 'media_check_spec') {
        return Promise.resolve({
          machine: 'tower',
          invite: 'legion-invite-1.fake',
          image_path: '/home/me/a.png',
          video_path: '/home/me/b.mp4',
          verify_resume: false,
        });
      }
      if (command === 'pair') {
        return Promise.reject({
          code: 'timeout',
          message: 'Timed out waiting for tower to confirm enrolment after 45s.',
        });
      }
      if (command === 'media_check_report') {
        failReports.push(payload.report);
        if (payload.report.error) failResolve(payload.report);
        return Promise.resolve();
      }
      return Promise.resolve({});
    };
    const failLink = createAppLink();
    await failLink.state();
    const failReport = await failSeen;
    assert.equal(failReport.stage, 'pairing');
    assert.equal(failReport.pair_ok, false);
    assert.equal(failReport.ok, false);
    assert.match(failReport.error, /45s/);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevDocument === undefined) delete globalThis.document;
    else globalThis.document = prevDocument;
    if (prevImage === undefined) delete globalThis.Image;
    else globalThis.Image = prevImage;
  }
});

test('createAppLink restorePairing() invokes restore_pairing with existingPrincipal and refreshes state', async () => {
  const prevWindow = globalThis.window;
  const calls = [];
  globalThis.window = {
    location: { origin: 'tauri://localhost' },
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'restore_pairing') {
            return Promise.resolve({
              ready: true,
              nodes: [{ node: 'tower' }],
              last_paired: { node: 'tower', device: 'phone', role: 'operator', synced: [], not_synced: [] },
            });
          }
          if (command === 'state') {
            return Promise.resolve({ ready: true, nodes: [{ node: 'tower', via: 'ssh' }] });
          }
          if (command === 'call' && payload?.tool === 'meta.whoami') {
            return Promise.resolve({ node: 'tower', principal: 'device:phone', role: 'operator', tools: [] });
          }
          if (command === 'call' && payload?.tool === 'meta.version') {
            return Promise.resolve({ version: '0.1.0', protocol: 1 });
          }
          return Promise.resolve({});
        },
      },
    },
  };

  try {
    const link = createAppLink();
    const res = await link.restorePairing('legion-invite-1.fake', 'device:phone', 'phone');
    assert.equal(res.ready, true);
    assert.equal(res.last_paired?.node, 'tower');
    assert.deepEqual(calls[0], {
      command: 'restore_pairing',
      payload: {
        invite: 'legion-invite-1.fake',
        existingPrincipal: 'device:phone',
        name: 'phone',
      },
    });
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
  }
});

test('createAppLink exposes nativeAudio only on Android, asks notification permission once on first play, and forwards state events', async () => {
  const prevWindow = globalThis.window;
  const prevNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  const calls = [];
  const listeners = new Map();

  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { userAgent: 'Mozilla/5.0 (Linux; Android 16; Pixel 10) AppleWebKit/537.36' },
  });
  globalThis.window = {
    location: { origin: 'http://tauri.localhost' },
    __TAURI__: {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'audio_request_notification_permission') {
            return Promise.resolve(false);
          }
          if (command === 'audio_state') {
            return Promise.resolve({ loaded: true, playing: true, path: '/music/01.mp3' });
          }
          return Promise.resolve({ ok: true });
        },
      },
      event: {
        listen(name, handler) {
          listeners.set(name, handler);
          return Promise.resolve(() => listeners.delete(name));
        },
      },
    },
  };

  try {
    const link = createAppLink();
    assert.ok(link.nativeAudio, 'Android app link must expose nativeAudio');

    const seen = [];
    const unsub = link.nativeAudio.onState((st) => seen.push(st));
    await Promise.resolve();
    listeners.get('legion://audio-state')?.({ payload: { loaded: true, playing: false, path: '/music/01.mp3' } });
    assert.equal(seen.length, 1);
    assert.equal(seen[0].path, '/music/01.mp3');

    // Loading paused does not ask notification permission yet.
    await link.nativeAudio.loadQueue({ machine: 'tower', tracks: [{ path: '/music/01.mp3', name: '01.mp3' }], startIndex: 0, autoPlay: false });
    assert.equal(calls.filter((c) => c.command === 'audio_request_notification_permission').length, 0);

    // First play asks notification permission once and still sends play even if refused.
    await link.nativeAudio.command('play');
    await link.nativeAudio.command('play');
    assert.equal(calls.filter((c) => c.command === 'audio_request_notification_permission').length, 1);
    assert.equal(calls.filter((c) => c.command === 'audio_command' && c.payload?.action === 'play').length, 2);

    const st = await link.nativeAudio.getState();
    assert.equal(st.playing, true);
    await link.nativeAudio.setModes({ shuffle: true, repeat: 'folder', order: ['/music/01.mp3'] });
    await link.nativeAudio.dismiss();
    unsub();

    // Desktop app link omits nativeAudio.
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { userAgent: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36' },
    });
    const desktopLink = createAppLink();
    assert.equal(desktopLink.nativeAudio, undefined);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevNavigator) Object.defineProperty(globalThis, 'navigator', prevNavigator);
    else delete globalThis.navigator;
  }
});

test('createAppLink exposes iosAudio when iOS legionAudio handler exists and routes Now Playing & remote commands', () => {
  const prevWindow = globalThis.window;
  const prevNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  const posted = [];

  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15' },
  });
  globalThis.window = {
    location: { origin: 'tauri://localhost' },
    webkit: {
      messageHandlers: {
        legionAudio: {
          postMessage(msg) {
            posted.push(msg);
          },
        },
      },
    },
    __TAURI__: { core: { invoke: () => Promise.resolve({}) } },
  };

  try {
    const link = createAppLink();
    assert.ok(link.iosAudio, 'iOS app link must expose iosAudio when handler is attached');
    assert.equal(link.nativeAudio, undefined);

    const received = [];
    const unsub = link.iosAudio.onRemoteCommand((cmd) => received.push(cmd));
    link.iosAudio.activateSession();
    link.iosAudio.updateNowPlaying({ title: 'Track 1', artist: 'Clara', playing: true, canNext: true });
    link.iosAudio.updatePlaybackState({ elapsed: 12, duration: 60, playing: true, rate: 1, canNext: true });
    globalThis.window.__LEGION_IOS_AUDIO_DISPATCH__?.({ action: 'next' });
    globalThis.window.__LEGION_IOS_AUDIO_DISPATCH__?.({ action: 'route_unavailable' });
    link.iosAudio.clearNowPlaying();
    unsub();

    assert.deepEqual(
      posted.map((m) => m.type),
      ['activate', 'now_playing', 'playback_state', 'clear'],
    );
    assert.deepEqual(received, [{ action: 'next' }, { action: 'route_unavailable' }]);
    assert.equal(globalThis.window.__LEGION_IOS_AUDIO_DISPATCH__, undefined);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevNavigator) Object.defineProperty(globalThis, 'navigator', prevNavigator);
    else delete globalThis.navigator;
  }
});
