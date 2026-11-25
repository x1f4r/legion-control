import assert from 'node:assert/strict';
import test from 'node:test';
import { createMockLink } from '../../link/mock.js';
import { detectFileCapabilities } from '../files.js';

test('mock link exposes all new Files tools, sample media, favourites, sync, and device transfer queue', async () => {
  const link = createMockLink();
  const tools = await link.tools({ on: 'tower' });
  const caps = detectFileCapabilities({ tools, link });
  assert.equal(caps.hasThumb, true);
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

  const thumbs = await link.call('fs.thumb', { paths: ['/home/me/Videos/Holiday/IMG_2041.jpg'] }, { on: 'tower' });
  assert.equal(thumbs.thumbnails['/home/me/Videos/Holiday/IMG_2041.jpg'].mime, 'image/svg+xml');

  const vid = await link.contentUrl({ machine: 'tower', path: '/home/me/Videos/Holiday/Arrival.mp4' });
  assert.equal(vid.mime, 'video/mp4');
  assert.ok(vid.url);

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
  await assert.rejects(() => link.call('fs.thumb', { paths: ['/home/me/Videos/Holiday/IMG_2041.jpg'] }, { on: 'tower' }));
  await assert.rejects(() => link.call('fav.list', {}, { on: 'tower' }));
});

test('mock link with large flag pages 5 000 entries cleanly with offset', async () => {
  const link = createMockLink('large');
  const page1 = await link.call('fs.list', { path: '/home/me/Archive5000', offset: 0, limit: 200 }, { on: 'tower' });
  assert.equal(page1.total, 5000);
  assert.equal(page1.entries.length, 200);
  assert.equal(page1.truncated, true);
  const pageLast = await link.call('fs.list', { path: '/home/me/Archive5000', offset: 4900, limit: 200 }, { on: 'tower' });
  assert.equal(pageLast.entries.length, 100);
  assert.equal(pageLast.truncated, false);
});
