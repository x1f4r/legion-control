import assert from 'node:assert/strict';
import test from 'node:test';

import { createHttpLink } from '../../link/http.js';
import { createMockLink } from '../../link/mock.js';

function withMockEnv(fetchImpl, initialToken = 'lgn_test_token') {
  const prevFetch = globalThis.fetch;
  const prevStorage = globalThis.localStorage;
  const store = new Map();
  if (initialToken) store.set('legion.token', initialToken);
  globalThis.localStorage = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
  };
  globalThis.fetch = fetchImpl;
  return () => {
    if (prevFetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = prevFetch;
    if (prevStorage === undefined) delete globalThis.localStorage;
    else globalThis.localStorage = prevStorage;
  };
}

test('createHttpLink contentUrl calls POST /v1/content and supports machine and download', async () => {
  const seen = [];
  const restore = withMockEnv(async (url, init) => {
    seen.push({ url, init });
    return {
      ok: true,
      status: 200,
      async json() {
        return { url: '/v1/content/ticket123/clip.mp4', expires_in: 900 };
      },
    };
  });

  try {
    const link = createHttpLink();
    const local = await link.contentUrl({ path: '/home/me/clip.mp4' });
    assert.deepEqual(local, { url: '/v1/content/ticket123/clip.mp4', expires_in: 900 });
    assert.equal(seen[0].url, '/v1/content');
    assert.equal(seen[0].init.method, 'POST');
    assert.equal(seen[0].init.headers.Authorization, 'Bearer lgn_test_token');
    assert.deepEqual(JSON.parse(seen[0].init.body), { path: '/home/me/clip.mp4' });

    const remoteDownload = await link.contentUrl({
      machine: 'pi',
      path: '~/media/clip.mp4',
      download: true,
    });
    assert.deepEqual(remoteDownload, {
      url: '/v1/content/ticket123/clip.mp4?download=1',
      expires_in: 900,
    });
    assert.deepEqual(JSON.parse(seen[1].init.body), { path: '~/media/clip.mp4', on: 'pi' });
  } finally {
    restore();
  }
});

test('createHttpLink contentUrl surfaces auth, argument, server, and network errors', async () => {
  const restoreNoToken = withMockEnv(async () => {
    throw new Error('fetch should not be called');
  }, '');
  try {
    const link = createHttpLink();
    await assert.rejects(() => link.contentUrl({ path: '/home/me/a.txt' }), {
      code: 'denied',
      status: 401,
    });
  } finally {
    restoreNoToken();
  }

  const restoreDenied = withMockEnv(async () => ({
    ok: false,
    status: 403,
    async json() {
      return { code: 'denied', message: 'Path is outside allowed roots.', machine: 'tower' };
    },
  }));
  try {
    const link = createHttpLink();
    await assert.rejects(() => link.contentUrl({ path: '' }), { code: 'invalid_args' });
    await assert.rejects(() => link.contentUrl({ path: '/etc/shadow' }), {
      code: 'denied',
      status: 403,
      machine: 'tower',
    });
  } finally {
    restoreDenied();
  }

  const restoreAbort = withMockEnv(async () => {
    const err = new Error('aborted');
    err.name = 'AbortError';
    throw err;
  });
  try {
    const link = createHttpLink();
    await assert.rejects(() => link.contentUrl({ path: '/home/me/a.txt' }), {
      code: 'cancelled',
    });
  } finally {
    restoreAbort();
  }
});

test('createHttpLink reads content support from /v1/health and remembers unsupported remote machines', async () => {
  const prevWindow = globalThis.window;
  globalThis.window = { location: { origin: 'http://127.0.0.1:4433' } };
  const restore = withMockEnv(async (url, init) => {
    if (url === '/v1/health') {
      return {
        ok: true,
        status: 200,
        async json() {
          return { ok: true, node: 'tower', version: '0.1.0', content: true };
        },
      };
    }
    if (url === '/v1/call') {
      const req = JSON.parse(init.body);
      const headers = { get: () => 'application/json' };
      if (req.tool === 'meta.whoami') {
        return {
          ok: true,
          status: 200,
          headers,
          async json() {
            return { node: 'tower', principal: 'ui', role: 'operator' };
          },
        };
      }
      return {
        ok: true,
        status: 200,
        headers,
        async json() {
          return { version: '0.1.0', protocol: 1 };
        },
      };
    }
    if (url === '/v1/content') {
      const req = JSON.parse(init.body);
      if (req.on === 'legacy') {
        return {
          ok: false,
          status: 501,
          async json() {
            return {
              code: 'unsupported',
              message: 'legacy answered stream get without modification time.',
              machine: 'legacy',
            };
          },
        };
      }
      return {
        ok: true,
        status: 200,
        async json() {
          return { url: '/v1/content/tok/pic.png', expires_in: 900 };
        },
      };
    }
    throw new Error(`unexpected fetch ${url}`);
  });

  try {
    const link = createHttpLink();
    let notifications = 0;
    const unsub = link.onContentSupportChange(() => {
      notifications += 1;
    });
    assert.equal(link.supportsContent, false);
    assert.equal(link.supportsContentFor('tower'), false);

    const st = await link.state();
    assert.equal(st.ready, true);
    assert.equal(link.supportsContent, true);
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(link.supportsContentFor('pi'), true);
    assert.equal(link.supportsContentFor('legacy'), true);
    assert.equal(notifications, 1);

    await assert.rejects(
      () => link.contentUrl({ machine: 'legacy', path: '~/media/pic.png' }),
      { code: 'unsupported', status: 501, machine: 'legacy' },
    );
    assert.equal(link.supportsContentFor('legacy'), false);
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(link.supportsContentFor('pi'), true);
    assert.equal(notifications, 2);

    await link.forget();
    assert.equal(link.supportsContentFor('legacy'), true);
    unsub();
  } finally {
    restore();
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
  }
});

test('createHttpLink clears remembered unsupported state when capabilities refresh', async () => {
  const prevWindow = globalThis.window;
  globalThis.window = { location: { origin: 'http://127.0.0.1:4433' } };
  let doorHasContent = false;
  let legacyUpgraded = false;

  const restore = withMockEnv(async (url, init) => {
    const jsonHeaders = { get: () => 'application/json' };
    if (url === '/v1/health') {
      return {
        ok: true,
        status: 200,
        async json() {
          return { ok: true, node: 'tower', version: '0.2.0', content: doorHasContent };
        },
      };
    }
    if (url === '/v1/tools?all=1') {
      return {
        ok: true,
        status: 200,
        async json() {
          return { tools: [{ name: 'meta.version' }] };
        },
      };
    }
    if (url === '/v1/call') {
      const req = JSON.parse(init.body);
      if (req.tool === 'meta.whoami') {
        return {
          ok: true,
          status: 200,
          headers: jsonHeaders,
          async json() {
            return { node: req.on || 'tower', principal: 'ui', role: 'operator' };
          },
        };
      }
      if (req.tool === 'meta.version') {
        if (req.on === 'all') {
          return {
            ok: true,
            status: 200,
            headers: jsonHeaders,
            async json() {
              return {
                results: {
                  legacy: { ok: { version: '0.2.0', protocol: 1 } },
                  other_old: { error: { code: 'unreachable', message: 'offline' } },
                },
              };
            },
          };
        }
        return {
          ok: true,
          status: 200,
          headers: jsonHeaders,
          async json() {
            return { version: '0.2.0', protocol: 1 };
          },
        };
      }
      if (req.tool === 'fs.list') {
        return {
          ok: true,
          status: 200,
          headers: jsonHeaders,
          async json() {
            return { entries: [] };
          },
        };
      }
    }
    if (url === '/v1/content') {
      const req = JSON.parse(init.body);
      if ((req.on === 'legacy' && !legacyUpgraded) || req.on === 'other_old') {
        return {
          ok: false,
          status: 501,
          async json() {
            return {
              code: 'unsupported',
              message: `${req.on} does not support ranged content streams.`,
              machine: req.on,
            };
          },
        };
      }
      if (req.on === 'flaky') {
        return {
          ok: false,
          status: 503,
          async json() {
            return { code: 'busy', message: 'Too many open streams.', machine: 'flaky' };
          },
        };
      }
      return {
        ok: true,
        status: 200,
        async json() {
          return { url: '/v1/content/tok/pic.png', expires_in: 900 };
        },
      };
    }
    throw new Error(`unexpected fetch ${url}`);
  });

  try {
    const link = createHttpLink();
    let notifications = 0;
    const unsub = link.onContentSupportChange(() => {
      notifications += 1;
    });

    // 1. Door initially reports content: false, then upgrades behind /v1/health.
    await link.state();
    assert.equal(link.supportsContent, false);
    assert.equal(link.supportsContentFor('tower'), false);

    doorHasContent = true;
    await link.state();
    assert.equal(link.supportsContent, true);
    assert.equal(link.supportsContentFor('tower'), true);
    assert.equal(notifications, 1);

    // 2. Transient errors (busy/503) do not mark a machine unsupported.
    await assert.rejects(() => link.contentUrl({ machine: 'flaky', path: '~/pic.png' }), {
      code: 'busy',
    });
    assert.equal(link.supportsContentFor('flaky'), true);

    // 3. Unsupported remote machines are remembered across ordinary non-capability calls (fs.list).
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '~/pic.png' }), {
      code: 'unsupported',
    });
    await assert.rejects(() => link.contentUrl({ machine: 'other_old', path: '~/pic.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    assert.equal(link.supportsContentFor('other_old'), false);

    await link.call('fs.list', { path: '~' }, { on: 'legacy' });
    assert.equal(
      link.supportsContentFor('legacy'),
      false,
      'ordinary non-capability calls must not clear remembered unsupported state',
    );

    // 4. Fan-out capability refresh clears only machines that answered ok, leaving failing ones marked.
    await link.call('meta.version', {}, { on: 'all' });
    assert.equal(link.supportsContentFor('legacy'), true);
    assert.equal(link.supportsContentFor('other_old'), false);

    // After upgrade, contentUrl succeeds without reloading the page.
    legacyUpgraded = true;
    const res = await link.contentUrl({ machine: 'legacy', path: '~/pic.png' });
    assert.equal(res.url, '/v1/content/tok/pic.png');

    // 5. Single-machine capability call clears unsupported for that machine too.
    await link.call('meta.whoami', {}, { on: 'other_old' });
    assert.equal(link.supportsContentFor('other_old'), true);

    // 6. Door-local unsupported state clears on tools() or /v1/health.
    legacyUpgraded = false;
    await assert.rejects(() => link.contentUrl({ machine: 'legacy', path: '~/pic.png' }), {
      code: 'unsupported',
    });
    assert.equal(link.supportsContentFor('legacy'), false);
    legacyUpgraded = true;
    // Direct successful contentUrl also clears unsupported.
    await link.contentUrl({ machine: 'legacy', path: '~/pic.png' });
    assert.equal(link.supportsContentFor('legacy'), true);

    unsub();
  } finally {
    restore();
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
  }
});

test('createMockLink contentUrl serves built-in samples and enforces auth, reachability, and read policy', async () => {
  const link = createMockLink();

  const known = await link.contentUrl({ path: '~/docs/hello.txt' });
  assert.equal(known.expires_in, 900);
  assert.ok(known.url.startsWith('data:text/plain;charset=utf-8,'));
  assert.match(decodeURIComponent(known.url.split(',')[1]), /Hello from Legion mock file preview/);

  const html = await link.contentUrl({ path: '/home/me/page.html' });
  assert.ok(html.url.startsWith('data:text/plain;charset=utf-8,'));

  const png = await link.contentUrl({ path: '/home/me/photo.jpg' });
  assert.ok(png.url.startsWith('data:image/png;base64,'));

  const svg = await link.contentUrl({ path: '/home/me/icon.svg' });
  assert.ok(svg.url.startsWith('data:image/svg+xml;charset=utf-8,'));

  const video = await link.contentUrl({ machine: 'pi', path: '/home/me/clip.webm' });
  assert.ok(video.url.startsWith('data:video/webm;base64,') || video.url.startsWith('blob:'));

  const audio = await link.contentUrl({ path: '/home/me/song.wav' });
  assert.ok(audio.url.startsWith('data:audio/ogg;base64,') || audio.url.startsWith('blob:'));

  const vtt = await link.contentUrl({ path: '/home/me/sub.vtt' });
  assert.ok(vtt.url.startsWith('data:text/vtt;charset=utf-8,'));

  const pdf = await link.contentUrl({ path: '/home/me/doc.pdf', download: true });
  assert.ok(pdf.url.startsWith('data:application/pdf;base64,'));
  assert.ok(!pdf.url.includes('?download=1'), 'data URL must stay valid base64');

  const bin = await link.contentUrl({ path: '/home/me/archive.bin' });
  assert.ok(bin.url.startsWith('data:application/octet-stream;base64,'));

  await assert.rejects(() => link.contentUrl({ machine: 'laptop', path: '/home/me/notes.txt' }), {
    code: 'unreachable',
  });
  await assert.rejects(() => link.contentUrl({ machine: 'ghost', path: '/home/me/notes.txt' }), {
    code: 'not_found',
  });

  const denied = createMockLink('none');
  await assert.rejects(() => denied.contentUrl({ path: '/home/me/notes.txt' }), {
    code: 'denied',
  });

  const unauthed = createMockLink('token');
  await assert.rejects(() => unauthed.contentUrl({ path: '/home/me/notes.txt' }), {
    code: 'denied',
    status: 401,
  });
});
