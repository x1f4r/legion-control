import assert from 'node:assert/strict';
import test from 'node:test';
import {
  baseFileName,
  buildTransferDestination,
  categoryFromName,
  changedPresetCutoff,
  detectFileCapabilities,
  entryCategory,
  entryIconName,
  filesFallbackBackRoute,
  filterEntries,
  folderDisplayName,
  formatFolderTotal,
  formatPathForDisplay,
  formatShowingCount,
  isHiddenName,
  isRetryableThumbError,
  isThumbEligible,
  joinFilePath,
  mimeFromName,
  parentFilePath,
  plainCategoryLabel,
  selectAllEntries,
  selectionSummary,
  shouldDefaultToGrid,
  sizePresetBounds,
  sortDescription,
  sortEntries,
  splitFileCrumbs,
  toggleSelection,
} from '../files.js';
import { createThumbCache, thumbCacheKey } from '../thumb-cache.js';
import {
  readFolderViewMode,
  readRecentDestinations,
  readRecentFiles,
  readSortPreference,
  readTextWrapPreference,
  recordRecentDestination,
  recordRecentFile,
  writeFolderViewMode,
  writeSortPreference,
  writeTextWrapPreference,
} from '../files-store.js';

function memoryStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
  };
}

test('categoryFromName and entryCategory map all design categories case-insensitively', () => {
  assert.equal(categoryFromName('IMG_2041.JPG'), 'image');
  assert.equal(categoryFromName('diagram.svg'), 'image');
  assert.equal(categoryFromName('photo.HEIC'), 'image');
  assert.equal(categoryFromName('Arrival.mp4'), 'video');
  assert.equal(categoryFromName('Concert.MKV'), 'video');
  assert.equal(categoryFromName('Voice note.m4a'), 'audio');
  assert.equal(categoryFromName('track.FLAC'), 'audio');
  assert.equal(categoryFromName('Tickets.pdf'), 'document');
  assert.equal(categoryFromName('budget.xlsx'), 'document');
  assert.equal(categoryFromName('notes.txt'), 'text');
  assert.equal(categoryFromName('Cargo.toml'), 'text');
  assert.equal(categoryFromName('main.rs'), 'text');
  assert.equal(categoryFromName('backup.tar.gz'), 'archive');
  assert.equal(categoryFromName('archive.7z'), 'archive');
  assert.equal(categoryFromName('mystery.bin'), 'other');
  assert.equal(categoryFromName('.bashrc'), 'other');
  assert.equal(categoryFromName('README'), 'other');

  // Server-provided category wins over extension; directories are always 'folder'.
  assert.equal(entryCategory({ kind: 'dir', name: 'photos.jpg' }), 'folder');
  assert.equal(entryCategory({ kind: 'file', name: 'raw', category: 'image' }), 'image');
  assert.equal(entryCategory({ kind: 'file', name: 'notes.md' }), 'text');

  assert.equal(plainCategoryLabel({ kind: 'dir', name: 'Raw' }), 'Folder');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.jpg' }), 'Picture');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.mp4' }), 'Video');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.m4a' }), 'Music');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.pdf' }), 'Document');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.txt' }), 'Text');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.zip' }), 'Archive');
  assert.equal(plainCategoryLabel({ kind: 'file', name: 'a.bin' }), 'File');

  assert.equal(entryIconName({ kind: 'dir' }), 'files');
  assert.equal(entryIconName({ kind: 'file', name: 'a.mp4' }), 'video');
  assert.equal(entryIconName({ kind: 'file', name: 'a.mp3' }), 'music');
  assert.equal(isThumbEligible({ kind: 'file', name: 'a.jpg' }), true);
  assert.equal(isThumbEligible({ kind: 'file', name: 'a.mp4' }), true);
  assert.equal(isThumbEligible({ kind: 'file', name: 'a.pdf' }), false);
  assert.equal(isThumbEligible({ kind: 'dir', name: 'Photos' }), false);
  assert.equal(mimeFromName('Arrival.mp4'), 'video/mp4');
  assert.equal(isHiddenName('.env'), true);
  assert.equal(isHiddenName('notes.txt'), false);
});

test('detectFileCapabilities cleanly degrades against old nodes and limited roles', () => {
  const oldTools = [
    { name: 'fs.list', effect: 'read', input_schema: { properties: { path: {}, depth: {}, hidden: {}, limit: {} } } },
    { name: 'fs.read', effect: 'read', input_schema: { properties: { path: {}, offset: {}, length: {} } } },
    { name: 'fs.find', effect: 'read', input_schema: { properties: { path: {}, glob: {}, kind: {} } } },
    { name: 'fs.transfer', effect: 'write', input_schema: { properties: { from: {}, to: {}, dry_run: {} } } },
  ];
  const oldCaps = detectFileCapabilities({
    tools: oldTools,
    allowedSet: new Set(oldTools.map((t) => t.name)),
    link: { kind: 'http', contentUrl: async () => ({ url: '' }), download: async () => ({ id: '1' }) },
  });
  assert.equal(oldCaps.hasList, true);
  assert.equal(oldCaps.hasListSort, false);
  assert.equal(oldCaps.hasListOffset, false);
  assert.equal(oldCaps.hasThumb, false);
  assert.equal(oldCaps.hasFav, false);
  assert.equal(oldCaps.hasSync, false);
  assert.equal(oldCaps.hasFindEnhanced, false);
  assert.equal(oldCaps.hasTransferBg, false);
  assert.equal(oldCaps.hasTransferMove, false);
  assert.equal(oldCaps.hasContentUrl, false);
  assert.equal(oldCaps.hasDownload, false);
  assert.equal(oldCaps.hasOpenExternally, false);
  assert.equal(oldCaps.hasUpload, false);

  // Unknown capabilities (empty tool list) assume the least, never the most.
  const unknownCaps = detectFileCapabilities({
    tools: [],
    allowedSet: null,
    link: { kind: 'http', supportsContent: true, contentUrl: async () => ({ url: '' }), download: async () => ({ id: '1' }) },
    machine: 'pi',
  });
  assert.equal(unknownCaps.known, false);
  assert.equal(unknownCaps.hasList, false);
  assert.equal(unknownCaps.hasRead, false);
  assert.equal(unknownCaps.hasListSort, false);
  assert.equal(unknownCaps.hasContentUrl, false);

  // Per-machine content support via `supportsContentFor(machine)` enables real `legion serve`
  // on current-version nodes even without `fs.thumb`, while disabling older nodes and machines
  // remembered as unsupported.
  const perMachineLink = {
    kind: 'http',
    supportsContent: true,
    supportsContentFor: (m) => m !== 'legacy',
    contentUrl: async () => ({ url: '' }),
    download: async () => ({ id: '1' }),
  };
  const currentWithoutThumb = [
    { name: 'fs.list', effect: 'read', input_schema: { properties: { path: {}, depth: {}, hidden: {}, limit: {}, offset: {} } } },
    { name: 'fs.read', effect: 'read', input_schema: { properties: { path: {} } } },
  ];
  const liveCaps = detectFileCapabilities({
    tools: currentWithoutThumb,
    allowedSet: new Set(currentWithoutThumb.map((t) => t.name)),
    link: perMachineLink,
    machine: 'pi',
  });
  assert.equal(liveCaps.hasContentUrl, true);
  assert.equal(liveCaps.hasDownload, true);
  const oldNodeCaps = detectFileCapabilities({
    tools: oldTools,
    allowedSet: new Set(oldTools.map((t) => t.name)),
    link: perMachineLink,
    machine: 'pi',
  });
  assert.equal(oldNodeCaps.hasContentUrl, false);
  assert.equal(oldNodeCaps.hasDownload, false);
  const legacyCaps = detectFileCapabilities({
    tools: currentWithoutThumb,
    allowedSet: new Set(currentWithoutThumb.map((t) => t.name)),
    link: perMachineLink,
    machine: 'legacy',
  });
  assert.equal(legacyCaps.hasContentUrl, false);
  assert.equal(legacyCaps.hasDownload, false);

  const newTools = [
    { name: 'fs.list', effect: 'read', input_schema: { properties: { path: {}, sort: {}, order: {}, offset: {}, categories: {} } } },
    { name: 'fs.read', effect: 'read', input_schema: { properties: { path: {} } } },
    { name: 'fs.find', effect: 'read', input_schema: { properties: { path: {}, name: {}, categories: {} } } },
    { name: 'fs.thumb', effect: 'read', input_schema: { properties: { paths: {} } } },
    { name: 'fs.mkdir', effect: 'write', input_schema: { properties: { path: {} } } },
    { name: 'fs.move', effect: 'write', input_schema: { properties: { from: {}, to: {} } } },
    { name: 'fs.remove', effect: 'destroy', input_schema: { properties: { path: {} } } },
    { name: 'fs.transfer', effect: 'write', input_schema: { properties: { from: {}, to: {}, background: {}, move: {}, if_exists: {} } } },
    { name: 'fav.list', effect: 'read', input_schema: { properties: {} } },
    { name: 'fav.add', effect: 'write', input_schema: { properties: {} } },
    { name: 'fav.remove', effect: 'write', input_schema: { properties: {} } },
    { name: 'sync.list', effect: 'read', input_schema: { properties: {} } },
  ];
  const fullCaps = detectFileCapabilities({
    tools: newTools,
    allowedSet: new Set(newTools.map((t) => t.name)),
    link: {
      kind: 'mock',
      contentUrl: async () => ({ url: '' }),
      download: async () => ({ id: '1' }),
      openExternally: async () => ({ ok: true }),
      upload: async () => ({ id: '2' }),
      transfers: async () => [],
    },
  });
  assert.equal(fullCaps.hasListSort, true);
  assert.equal(fullCaps.hasListOffset, true);
  assert.equal(fullCaps.hasThumb, true);
  assert.equal(fullCaps.hasFav, true);
  assert.equal(fullCaps.hasFavWrite, true);
  assert.equal(fullCaps.hasSync, true);
  assert.equal(fullCaps.hasTransferBg, true);
  assert.equal(fullCaps.hasTransferMove, true);
  assert.equal(fullCaps.hasContentUrl, true);
  assert.equal(fullCaps.hasDownload, true);
  assert.equal(fullCaps.hasOpenExternally, true);
  assert.equal(fullCaps.hasUpload, true);
  assert.equal(fullCaps.hasDeviceTransfers, true);

  // Node with background & if_exists on fs.transfer, but without cross-machine move advertised.
  const copyOnlyTools = newTools.map((t) =>
    t.name === 'fs.transfer'
      ? { ...t, input_schema: { properties: { from: {}, to: {}, background: {}, if_exists: {} } } }
      : t,
  );
  const copyOnlyCaps = detectFileCapabilities({
    tools: copyOnlyTools,
    allowedSet: new Set(copyOnlyTools.map((t) => t.name)),
    link: { kind: 'http', contentUrl: async () => ({ url: '' }) },
  });
  assert.equal(copyOnlyCaps.hasTransfer, true);
  assert.equal(copyOnlyCaps.hasTransferBg, true);
  assert.equal(copyOnlyCaps.hasTransferIfExists, true);
  assert.equal(copyOnlyCaps.hasTransferMove, false);
  assert.equal(copyOnlyCaps.hasMove, true);

  // Observer role has read tools only: write/destroy/move capabilities are off.
  const obsCaps = detectFileCapabilities({
    tools: newTools,
    allowedSet: new Set(['fs.list', 'fs.read', 'fs.thumb', 'fav.list']),
    link: { kind: 'mock', contentUrl: async () => ({ url: '' }) },
  });
  assert.equal(obsCaps.hasFav, true);
  assert.equal(obsCaps.hasFavWrite, false);
  assert.equal(obsCaps.hasRemove, false);
  assert.equal(obsCaps.hasTransfer, false);
});

test('sortEntries keeps directories first and sorts by name, modified, and size', () => {
  const sample = [
    { name: 'z.txt', kind: 'file', size: 100, mtime: '2026-06-01T10:00:00Z' },
    { name: 'Beta', kind: 'dir', size: 4096, mtime: '2026-06-03T10:00:00Z' },
    { name: 'alpha.jpg', kind: 'file', size: 5000, mtime: '2026-06-05T10:00:00Z' },
    { name: 'Alpha', kind: 'dir', size: 4096, mtime: '2026-06-02T10:00:00Z' },
  ];

  assert.deepEqual(
    sortEntries(sample, { sort: 'name', order: 'asc' }).map((e) => e.name),
    ['Alpha', 'Beta', 'alpha.jpg', 'z.txt'],
  );
  assert.deepEqual(
    sortEntries(sample, { sort: 'name', order: 'desc' }).map((e) => e.name),
    ['Beta', 'Alpha', 'z.txt', 'alpha.jpg'],
  );
  assert.deepEqual(
    sortEntries(sample, { sort: 'size', order: 'desc' }).map((e) => e.name),
    ['Alpha', 'Beta', 'alpha.jpg', 'z.txt'],
  );
  assert.deepEqual(
    sortEntries(sample, { sort: 'modified', order: 'desc' }).map((e) => e.name),
    ['Beta', 'Alpha', 'alpha.jpg', 'z.txt'],
  );
  assert.equal(sortDescription({ sort: 'modified', order: 'desc' }), 'newest first');
  assert.equal(sortDescription({ sort: 'size', order: 'desc' }), 'largest first');
  assert.equal(sortDescription({ sort: 'name', order: 'asc' }), 'by name');
});

test('filterEntries, shouldDefaultToGrid, and selection helpers behave accurately', () => {
  const entries = [
    { name: 'Raw', path: '/a/Raw', kind: 'dir', size: 4096, mtime: '2026-06-01T00:00:00Z' },
    { name: '.hidden', path: '/a/.hidden', kind: 'file', size: 12, mtime: '2026-06-01T00:00:00Z' },
    { name: 'IMG_1.jpg', path: '/a/IMG_1.jpg', kind: 'file', size: 4000, mtime: '2026-06-10T00:00:00Z' },
    { name: 'Arrival.mp4', path: '/a/Arrival.mp4', kind: 'file', size: 1200000, mtime: '2026-06-12T00:00:00Z' },
    { name: 'notes.txt', path: '/a/notes.txt', kind: 'file', size: 200, mtime: '2026-06-02T00:00:00Z' },
  ];

  assert.equal(filterEntries(entries, { hidden: false }).length, 4);
  assert.equal(filterEntries(entries, { hidden: true }).length, 5);
  assert.deepEqual(
    filterEntries(entries, { categories: ['video'] }).map((e) => e.name),
    ['Arrival.mp4'],
  );
  assert.deepEqual(
    filterEntries(entries, { query: 'arr mp4' }).map((e) => e.name),
    ['Arrival.mp4'],
  );
  assert.equal(shouldDefaultToGrid(entries), true);
  assert.equal(
    shouldDefaultToGrid([
      { kind: 'file', name: 'a.txt' },
      { kind: 'file', name: 'b.pdf' },
      { kind: 'file', name: 'c.jpg' },
    ]),
    false,
  );

  let sel = new Set();
  sel = toggleSelection(sel, '/a/IMG_1.jpg');
  sel = toggleSelection(sel, '/a/Arrival.mp4');
  const sum = selectionSummary(entries, sel);
  assert.equal(sum.count, 2);
  assert.equal(sum.fileCount, 2);
  assert.equal(sum.totalBytes, 1204000);
  assert.equal(sum.allSizesKnown, true);

  // Including a folder makes total size unknown so the UI hides a misleading size line.
  const selWithDir = toggleSelection(sel, '/a/Raw');
  const sumWithDir = selectionSummary(entries, selWithDir);
  assert.equal(sumWithDir.count, 3);
  assert.equal(sumWithDir.folderCount, 1);
  assert.equal(sumWithDir.allSizesKnown, false);

  sel = toggleSelection(sel, '/a/IMG_1.jpg');
  assert.equal(sel.size, 1);
  assert.equal(selectAllEntries(entries).size, 5);

  assert.equal(formatFolderTotal(5000, true), '5,000 items');
  assert.equal(formatFolderTotal(5000, false), 'More than 5,000 items');
  assert.equal(formatFolderTotal(1, true), '1 item');
  assert.equal(formatShowingCount(120, 5000, true), 'Showing 120 of 5,000');
  assert.equal(formatShowingCount(120, 5000, false), 'Showing 120 of more than 5,000');

  assert.equal(isRetryableThumbError({ code: 'busy' }), true);
  assert.equal(isRetryableThumbError({ code: 'rate_limited' }), true);
  assert.equal(isRetryableThumbError({ code: 'internal', retryable: true }), true);
  assert.equal(isRetryableThumbError({ code: 'unsupported' }), false);
  assert.equal(isRetryableThumbError({ code: 'not_found' }), false);
  assert.equal(isRetryableThumbError(null), false);
});

test('path helpers, crumbs, and fallback history stepping handle Unix and Windows paths', () => {
  assert.equal(baseFileName('/home/me/Videos/Holiday'), 'Holiday');
  assert.equal(baseFileName('/'), '/');
  assert.equal(baseFileName('C:\\Users\\me\\Photos'), 'Photos');
  assert.equal(baseFileName('C:\\'), 'C:');

  assert.equal(parentFilePath('/home/me/Videos/Holiday'), '/home/me/Videos');
  assert.equal(parentFilePath('/home'), '/');
  assert.equal(parentFilePath('/'), null);
  assert.equal(parentFilePath('C:\\Users\\me\\Photos'), 'C:\\Users\\me');
  assert.equal(parentFilePath('C:\\Users'), 'C:\\');
  assert.equal(parentFilePath('C:\\'), null);

  assert.equal(joinFilePath('/home/me', 'Holiday'), '/home/me/Holiday');
  assert.equal(joinFilePath('/', 'etc'), '/etc');
  assert.equal(joinFilePath('C:\\Users\\me', 'Photos'), 'C:\\Users\\me\\Photos');
  assert.equal(buildTransferDestination('/home/me/Films', '/home/me/Videos/Arrival.mp4'), '/home/me/Films/Arrival.mp4');

  const plainCrumbs = splitFileCrumbs('tower', '/home/me/Videos/Holiday', { technical: false });
  assert.deepEqual(
    plainCrumbs.map((c) => c.label),
    ['tower', 'Home', 'Videos', 'Holiday'],
  );
  assert.equal(formatPathForDisplay('/home/me/Videos/Holiday'), 'Home › Videos › Holiday');
  assert.equal(formatPathForDisplay('/home/me/Videos/Holiday', { technical: true }), '/home/me/Videos/Holiday');
  assert.equal(folderDisplayName('/home/me'), 'Home');
  assert.equal(folderDisplayName('/home/me', { technical: true }), 'me');

  // Fallback history stepping: sheet -> selection -> viewer -> parent folder -> Files home -> leave.
  const folderRoute = { section: 'files', view: 'folder', machine: 'tower', path: '/home/me/Videos/Holiday', viewer: { machine: 'tower', path: '/home/me/Videos/Holiday/IMG_1.jpg' } };
  assert.deepEqual(filesFallbackBackRoute(folderRoute, { hasSheet: true }), { action: 'close-sheet' });
  assert.deepEqual(filesFallbackBackRoute(folderRoute, { selecting: true }), { action: 'end-select' });
  assert.deepEqual(filesFallbackBackRoute(folderRoute), {
    action: 'navigate',
    href: '#/files/tower//home/me/Videos/Holiday',
  });
  assert.deepEqual(
    filesFallbackBackRoute({ section: 'files', view: 'folder', machine: 'tower', path: '/home/me/Videos/Holiday' }),
    { action: 'navigate', href: '#/files/tower//home/me/Videos' },
  );
  assert.deepEqual(
    filesFallbackBackRoute({ section: 'files', view: 'folder', machine: 'tower', path: '/home/me' }),
    { action: 'navigate', href: '#/files' },
  );
  assert.deepEqual(filesFallbackBackRoute({ section: 'files', view: 'home' }), { action: 'leave-files' });

  assert.deepEqual(sizePresetBounds('small'), { min_size: null, max_size: 1024 * 1024 });
  assert.ok(changedPresetCutoff('week', Date.parse('2026-06-15T00:00:00Z')).startsWith('2026-06-08'));
});

test('thumb cache enforces LRU bounds and files-store persists device preferences', () => {
  const storage = memoryStorage();
  const cache = createThumbCache({ maxEntries: 2, storage });
  const k1 = thumbCacheKey('tower', '/a.jpg', '1');
  const k2 = thumbCacheKey('tower', '/b.jpg', '1');
  const k3 = thumbCacheKey('tower', '/c.jpg', '1');
  cache.set(k1, { url: 'data:image/png;base64,AAA' });
  cache.set(k2, { url: 'data:image/png;base64,BBB' });
  assert.equal(cache.get(k1).url, 'data:image/png;base64,AAA'); // refreshes k1
  cache.set(k3, { url: 'data:image/png;base64,CCC' }); // evicts k2
  assert.equal(cache.size(), 2);
  assert.equal(cache.has(k1), true);
  assert.equal(cache.has(k2), false);
  assert.equal(cache.has(k3), true);

  recordRecentFile(storage, { machine: 'tower', path: '/home/me/notes.txt', name: 'notes.txt', size: 120 }, 1000);
  recordRecentFile(storage, { machine: 'tower', path: '/home/me/IMG.jpg', name: 'IMG.jpg', size: 400 }, 2000);
  recordRecentFile(storage, { machine: 'tower', path: '/home/me/notes.txt', name: 'notes.txt', size: 120 }, 3000);
  const recents = readRecentFiles(storage);
  assert.equal(recents.length, 2);
  assert.equal(recents[0].path, '/home/me/notes.txt');

  recordRecentDestination(storage, 'pi', '/home/me/Films');
  recordRecentDestination(storage, 'pi', '/home/me/Documents');
  assert.deepEqual(readRecentDestinations(storage, 'pi'), ['/home/me/Documents', '/home/me/Films']);

  writeFolderViewMode(storage, 'tower', '/home/me/Pictures', 'grid');
  assert.equal(readFolderViewMode(storage, 'tower', '/home/me/Pictures'), 'grid');

  writeSortPreference(storage, { sort: 'modified', order: 'desc' });
  assert.deepEqual(readSortPreference(storage), { sort: 'modified', order: 'desc' });

  assert.equal(readTextWrapPreference(storage), true);
  writeTextWrapPreference(storage, false);
  assert.equal(readTextWrapPreference(storage), false);
});
