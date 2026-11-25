import assert from 'node:assert/strict';
import test from 'node:test';
import { createHttpLink } from '../../link/http.js';
import { createMockLink } from '../../link/mock.js';
import { detectFileCapabilities } from '../files.js';

function withHttpMockEnv(fetchImpl, initialToken = 'lgn_test_token') {
  const prevFetch = globalThis.fetch;
  const prevStorage = globalThis.localStorage;
  const store = new Map();
  if (initialToken) store.set('legion.token', initialToken);
  globalThis.fetch = fetchImpl;
  globalThis.localStorage = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
  };
  return () => {
    if (prevFetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = prevFetch;
    if (prevStorage === undefined) delete globalThis.localStorage;
    else globalThis.localStorage = prevStorage;
  };
}

test('createHttpLink contentUrl matches POST /v1/content contract and returns {url, expires_in}', async () => {
  const seen = [];
  const restore = withHttpMockEnv(async (url, init) => {
    seen.push({ url, init });
    return {
      ok: true,
      status: 200,
      async json() {
        return { url: '/v1/content/ticket123/clip.webm', expires_in: 900 };
      },
    };
  });
  try {
    const link = createHttpLink();
    const local = await link.contentUrl({ path: '/home/me/clip.webm' });
    assert.deepEqual(local, { url: '/v1/content/ticket123/clip.webm', expires_in: 900 });
    assert.equal(seen[0].url, '/v1/content');
    assert.equal(seen[0].init.method, 'POST');
    assert.deepEqual(JSON.parse(seen[0].init.body), { path: '/home/me/clip.webm' });

    const remote = await link.contentUrl({ machine: 'pi', path: '~/clip.webm', download: true });
    assert.deepEqual(remote, { url: '/v1/content/ticket123/clip.webm?download=1', expires_in: 900 });
    assert.deepEqual(JSON.parse(seen[1].init.body), { path: '~/clip.webm', on: 'pi' });

    await assert.rejects(() => link.contentUrl({ path: '' }), { code: 'invalid_args' });
  } finally {
    restore();
  }
});

test('mock link exposes all new Files tools, royalty-free media, fs.copy, favourites, sync, and device transfer queue', async () => {
  const link = createMockLink();
  const tools = await link.tools({ on: 'tower' });
  const caps = detectFileCapabilities({ tools, link });
  assert.equal(caps.hasThumb, true);
  assert.equal(caps.hasCopy, true);
  assert.equal(caps.hasMove, true);
  assert.equal(caps.hasFav, true);
  assert.equal(caps.hasSync, true);
  assert.equal(caps.hasTransferBg, true);
  assert.equal(caps.hasContentUrl, true);
  assert.equal(caps.hasDownload, true);
  assert.equal(caps.hasUpload, true);
  assert.equal(caps.hasOpenExternally, true);
  assert.equal(caps.hasDeviceTransfers, true);

  const list = await link.call('fs.list', { path: '/home/me/Videos/Holiday', sort: 'size', order: 'desc' }, { on: 'tower' });
  assert.equal(list.entries[0].name, 'Raw'); // dir first
  assert.equal(list.entries[1].category, 'video');
  assert.equal(typeof list.total, 'number');
  assert.equal(list.total_exact, true);

  const thumbs = await link.call('fs.thumb', { paths: ['/home/me/Videos/Holiday/IMG_2041.jpg'] }, { on: 'tower' });
  assert.equal(thumbs.thumbnails['/home/me/Videos/Holiday/IMG_2041.jpg'].mime, 'image/svg+xml');

  const vid = await link.contentUrl({ machine: 'tower', path: '/home/me/Videos/Holiday/Arrival.mp4' });
  assert.equal(vid.expires_in, 900);
  assert.ok(vid.url.startsWith('data:video/webm;base64,') || vid.url.startsWith('blob:'));

  const aud = await link.contentUrl({ machine: 'tower', path: '/home/me/Videos/Holiday/Voice note.ogg' });
  assert.equal(aud.expires_in, 900);
  assert.ok(aud.url.startsWith('data:audio/ogg;base64,') || aud.url.startsWith('blob:'));

  // Auth, reachability, and read policy match origin/files/content mock behavior.
  await assert.rejects(() => link.contentUrl({ machine: 'laptop', path: '/home/me/notes.txt' }), { code: 'unreachable' });
  await assert.rejects(() => link.contentUrl({ machine: 'ghost', path: '/home/me/notes.txt' }), { code: 'not_found' });
  await assert.rejects(() => createMockLink('none').contentUrl({ path: '/home/me/notes.txt' }), { code: 'denied' });
  await assert.rejects(() => createMockLink('token').contentUrl({ path: '/home/me/notes.txt' }), { code: 'denied', status: 401 });

  // Same-machine fs.copy works instantaneously with no background job.
  await link.call('fs.copy', { from: '/home/me/Videos/Holiday/notes.txt', to: '/home/me/Documents/notes.txt' }, { on: 'tower' });
  const docsAfter = await link.call('fs.list', { path: '/home/me/Documents' }, { on: 'tower' });
  assert.ok(docsAfter.entries.some((e) => e.name === 'notes.txt'));

  const favs = await link.call('fav.list', {}, { on: 'tower' });
  assert.ok(favs.entries.some((f) => f.pinned));

  const added = await link.call('fav.add', { machine: 'tower', path: '/home/me/Videos/Holiday/Beach.mp4', pinned: true }, { on: 'tower' });
  assert.equal(added.entry.pinned, true);
  await link.call('fav.remove', { id: added.entry.id }, { on: 'tower' });

  // Non-blocking download returns immediately and supports pause/resume/cancel.
  const dl = await link.download({ machine: 'tower', path: '/home/me/Videos/Holiday/Beach.mp4', name: 'Beach.mp4', size: 840_000_000 });
  assert.ok(dl.id);
  const paused = await link.transferControl(dl.id, 'pause');
  assert.equal(paused.state, 'paused');
  const cancelled = await link.transferControl(dl.id, 'cancel');
  assert.equal(cancelled.state, 'cancelled');
});

test('mock link with old flag cleanly omits new tools and optional link methods', async () => {
  const link = createMockLink('old');
  const tools = await link.tools({ on: 'tower' });
  const caps = detectFileCapabilities({ tools, link });
  assert.equal(caps.hasThumb, false);
  assert.equal(caps.hasFav, false);
  assert.equal(caps.hasSync, false);
  assert.equal(caps.hasListOffset, false);
  assert.equal(caps.hasContentUrl, false);
  assert.equal(caps.hasDownload, false);
  assert.equal(caps.hasUpload, false);
  assert.equal(caps.hasOpenExternally, false);
  assert.equal(caps.hasDeviceTransfers, false);

  const list = await link.call('fs.list', { path: '/home/me/Videos/Holiday' }, { on: 'tower' });
  assert.equal(list.total, undefined);
  assert.equal(list.entries.find((e) => e.name === 'Arrival.mp4').category, undefined);
  await assert.rejects(
    () => link.call('fs.list', { path: '/home/me/Videos/Holiday', sort: 'name' }, { on: 'tower' }),
    (err) => err?.code === 'invalid_args' && /unknown field `sort`/.test(err.message),
  );
  await assert.rejects(() => link.call('fs.thumb', { paths: ['/home/me/Videos/Holiday/IMG_2041.jpg'] }, { on: 'tower' }));
  await assert.rejects(() => link.call('fav.list', {}, { on: 'tower' }));
});

test('mock link with large and inexact flags pages 5 000 entries and exposes total_exact', async () => {
  const link = createMockLink('large,inexact');
  const page1 = await link.call('fs.list', { path: '/home/me/Archive5000', offset: 0, limit: 200 }, { on: 'tower' });
  assert.equal(page1.total, 5000);
  assert.equal(page1.total_exact, false);
  assert.equal(page1.entries.length, 200);
  assert.equal(page1.truncated, true);
  const pageLast = await link.call('fs.list', { path: '/home/me/Archive5000', offset: 4900, limit: 200 }, { on: 'tower' });
  assert.equal(pageLast.entries.length, 100);
  assert.equal(pageLast.truncated, false);
});

test('mock link with thumb-retry returns retryable busy error once then succeeds', async () => {
  const link = createMockLink('thumb-retry');
  const first = await link.call('fs.thumb', { paths: ['/home/me/Videos/Holiday/IMG_2042.jpg'] }, { on: 'tower' });
  assert.equal(first.thumbnails['/home/me/Videos/Holiday/IMG_2042.jpg'].error.code, 'busy');
  assert.equal(first.thumbnails['/home/me/Videos/Holiday/IMG_2042.jpg'].error.retryable, true);
  const second = await link.call('fs.thumb', { paths: ['/home/me/Videos/Holiday/IMG_2042.jpg'] }, { on: 'tower' });
  assert.equal(second.thumbnails['/home/me/Videos/Holiday/IMG_2042.jpg'].mime, 'image/svg+xml');
});

test('mock link serves fs.tags, audio covers via fs.thumb up to 1024px, and records immersive calls', async () => {
  const link = createMockLink();
  const tools = await link.tools({ on: 'tower' });
  const caps = detectFileCapabilities({ tools, link });
  assert.equal(caps.hasTags, true);
  assert.equal(caps.hasAudioCovers, true);
  assert.equal(caps.thumbMaxSize, 1024);

  const tags = await link.call(
    'fs.tags',
    {
      paths: [
        '/home/me/Music/Album/01 - Morning Light.flac',
        '/home/me/Music/Album/field_recording_untagged.wav',
        '/home/me/notes.txt',
      ],
    },
    { on: 'tower' },
  );
  assert.equal(tags.results.length, 3);
  assert.equal(tags.results[0].ok.title, 'Morning Light');
  assert.equal(tags.results[0].ok.artist, 'Klara');
  assert.equal(tags.results[0].ok.album, 'Early Hours');
  assert.equal(tags.results[0].ok.cover, true);
  assert.deepEqual(tags.results[1].ok, { cover: false });
  assert.equal(tags.results[2].error.code, 'unsupported');

  const cover = await link.call(
    'fs.thumb',
    {
      paths: [
        '/home/me/Music/Album/01 - Morning Light.flac',
        '/home/me/Music/Album/field_recording_untagged.wav',
      ],
      size: 1024,
      fit: 'contain',
    },
    { on: 'tower' },
  );
  assert.equal(cover.results[0].ok.width, 1024);
  assert.equal(cover.results[1].error.code, 'not_found');

  assert.equal(link.canImmersive(), true);
  await link.immersive(true, { orientation: 'landscape' });
  await link.keepScreenOn(true);
  await link.immersive(false);
  await link.keepScreenOn(false);
  assert.equal(link.immersiveCalls.length, 2);
  assert.equal(link.immersiveCalls[0].on, true);
  assert.equal(link.immersiveCalls[0].orientation, 'landscape');
  assert.equal(link.immersiveCalls[1].on, false);
  assert.equal(link.keepScreenOnCalls.length, 2);

  // Old machine omits fs.tags and audio covers; no-fullscreen disables canImmersive.
  const oldLink = createMockLink('old');
  const oldCaps = detectFileCapabilities({ tools: await oldLink.tools({ on: 'tower' }), link: oldLink });
  assert.equal(oldCaps.hasTags, false);
  assert.equal(oldCaps.hasAudioCovers, false);
  await assert.rejects(() => oldLink.call('fs.tags', { paths: ['/home/me/Music/Album/01 - Morning Light.flac'] }, { on: 'tower' }));

  const noFsLink = createMockLink('no-fullscreen');
  assert.equal(noFsLink.canImmersive(), false);
});
