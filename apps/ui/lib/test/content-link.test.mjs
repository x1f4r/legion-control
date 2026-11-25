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
  assert.ok(video.url.startsWith('data:video/webm;base64,'));

  const audio = await link.contentUrl({ path: '/home/me/song.wav' });
  assert.ok(audio.url.startsWith('data:audio/wav;base64,'));

  const vtt = await link.contentUrl({ path: '/home/me/sub.vtt' });
  assert.ok(vtt.url.startsWith('data:text/vtt;charset=utf-8,'));

  const pdf = await link.contentUrl({ path: '/home/me/doc.pdf', download: true });
  assert.ok(pdf.url.startsWith('data:application/octet-stream;base64,'));
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
