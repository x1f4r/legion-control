import assert from 'node:assert/strict';
import test from 'node:test';
import {
  baseFileName,
  buildTransferDestination,
  canOpenExternally,
  categoryFromName,
  changedPresetCutoff,
  computeVirtualGridWindow,
  computeVirtualWindow,
  createThumbQueue,
  createVirtualListLayout,
  detectFileCapabilities,
  entryCategory,
  entryIconName,
  extractThumbMap,
  filesFallbackBackRoute,
  filterEntries,
  folderDisplayName,
  formatFolderTotal,
  formatPathForDisplay,
  formatShowingCount,
  isExtensionlessName,
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
  THUMB_BATCH_MAX,
  THUMB_MAX_RETRIES,
  THUMB_RETRY_BASE_MS,
  toggleSelection,
  viewerModeFromName,
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
  assert.equal(categoryFromName('Chapter.m4b'), 'audio');
  assert.equal(categoryFromName('track.FLAC'), 'audio');
  assert.equal(categoryFromName('master.aif'), 'audio');
  assert.equal(categoryFromName('master.AIFF'), 'audio');
  assert.equal(mimeFromName('Chapter.m4b'), 'audio/mp4');
  assert.equal(mimeFromName('master.aif'), 'audio/aiff');
  assert.equal(mimeFromName('master.AIFF'), 'audio/aiff');
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
  assert.equal(isThumbEligible({ kind: 'file', name: 'song.mp3' }), false);
  assert.equal(isThumbEligible({ kind: 'file', name: 'song.mp3' }, { allowAudio: true }), true);
  assert.equal(isThumbEligible({ kind: 'file', name: 'track.flac' }, { allowAudio: true }), true);
  assert.equal(isThumbEligible({ kind: 'file', name: 'a.pdf' }), false);
  assert.equal(isThumbEligible({ kind: 'file', name: 'a.pdf' }, { allowAudio: true }), false);
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
  assert.equal(oldCaps.hasTags, false);
  assert.equal(oldCaps.hasAudioThumb, false);
  assert.equal(oldCaps.thumbMaxSize, 0);
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
    { name: 'fs.thumb', effect: 'read', input_schema: { properties: { paths: {}, size: { maximum: 1024 } } } },
    { name: 'fs.tags', effect: 'read', input_schema: { properties: { paths: {} } } },
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
  assert.equal(fullCaps.hasTags, true);
  assert.equal(fullCaps.hasAudioThumb, true);
  assert.equal(fullCaps.thumbMaxSize, 1024);
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
  assert.equal(obsCaps.hasTags, false);
  assert.equal(obsCaps.hasAudioThumb, true);

  // Older node with fs.thumb (size max 512) and without fs.tags lacks tags and audio covers.
  const preAudioThumbCaps = detectFileCapabilities({
    tools: [
      { name: 'fs.list', effect: 'read', input_schema: { properties: { path: {}, offset: {} } } },
      { name: 'fs.read', effect: 'read', input_schema: { properties: { path: {} } } },
      { name: 'fs.thumb', effect: 'read', input_schema: { properties: { paths: {}, size: { maximum: 512 } } } },
    ],
    allowedSet: new Set(['fs.list', 'fs.read', 'fs.thumb']),
  });
  assert.equal(preAudioThumbCaps.hasThumb, true);
  assert.equal(preAudioThumbCaps.hasTags, false);
  assert.equal(preAudioThumbCaps.hasAudioThumb, false);
  assert.equal(preAudioThumbCaps.thumbMaxSize, 512);
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

test('viewerModeFromName, canOpenExternally, and isExtensionlessName follow canonical table', () => {
  // Playable media containers try the inline player.
  for (const name of [
    'track.flac',
    'song.mp3',
    'concert.m4a',
    'voice.ogg',
    'clip.opus',
    'sample.wav',
    'movie.mp4',
    'clip.webm',
    'archive.mkv',
    'camera.mov',
    'bluray.m2ts',
    'photo.jpg',
    'render.png',
    'modern.avif',
    'diagram.svg',
  ]) {
    assert.equal(viewerModeFromName(name), 'inline', `expected inline viewer for ${name}`);
  }

  // Known non-WebView pictures try fs.thumb preview before fallback.
  for (const name of ['shot.heic', 'shot.heif', 'scan.tif', 'scan.tiff', 'raw.dng', 'raw.cr2', 'raw.cr3', 'raw.nef', 'raw.arw', 'raw.raf', 'raw.orf', 'raw.rw2']) {
    assert.equal(categoryFromName(name), 'image', `expected image category for ${name}`);
    assert.equal(viewerModeFromName(name), 'thumb_or_fallback', `expected thumb_or_fallback for ${name}`);
    assert.equal(canOpenExternally(name), true, `expected external open allowed for ${name}`);
  }

  // Known non-WebView audio/video skip the player straight to fallback, while allowing external open.
  for (const name of ['old.wma', 'synth.mid', 'old.avi', 'old.wmv', 'old.flv', 'old.rm']) {
    assert.equal(viewerModeFromName(name), 'fallback', `expected fallback viewer for ${name}`);
    assert.equal(canOpenExternally(name), true, `expected external open allowed for ${name}`);
  }

  // Security-sensitive types, archives, web docs, SVG, scripts, executables, and extensionless files
  // must NEVER be offered for external opening.
  for (const blocked of [
    'diagram.svg',
    'index.html',
    'page.htm',
    'feed.xml',
    'app.js',
    'script.py',
    'run.sh',
    'installer.apk',
    'setup.exe',
    'archive.zip',
    'backup.tar.gz',
    'legacy.doc',
    'legacy.rtf',
    'README',
    '.gitignore',
    'mystery.unknownext',
  ]) {
    assert.equal(canOpenExternally(blocked), false, `expected ${blocked} to be refused for external open`);
  }

  // Safe documents & data formats allow external open.
  for (const allowed of ['manual.pdf', 'book.epub', 'memo.docx', 'sheet.xlsx', 'deck.pptx', 'doc.odt', 'data.csv', 'notes.txt', 'config.json', 'track.flac']) {
    assert.equal(canOpenExternally(allowed), true, `expected ${allowed} to allow external open`);
  }

  assert.equal(isExtensionlessName('README'), true);
  assert.equal(isExtensionlessName('/home/me/Makefile'), true);
  assert.equal(isExtensionlessName('.gitignore'), true);
  assert.equal(isExtensionlessName('notes.txt'), false);
  assert.equal(isExtensionlessName('archive.tar.gz'), false);
});

test('extractThumbMap converts real fs.thumb results and rejects legacy or malformed replies', () => {
  const okMap = extractThumbMap({
    results: [
      { path: '/home/me/a.jpg', ok: { mime: 'image/jpeg', content: 'QUFB', width: 128, height: 96 } },
      { path: '/home/me/b.png', ok: { mime: 'image/png', content: 'QkJC', width: 64, height: 64 } },
    ],
  });
  assert.equal(Object.getPrototypeOf(okMap), null);
  assert.deepEqual(okMap['/home/me/a.jpg'], { mime: 'image/jpeg', content: 'QUFB', width: 128, height: 96 });
  assert.deepEqual(okMap['/home/me/b.png'], { mime: 'image/png', content: 'QkJC', width: 64, height: 64 });

  const mixed = extractThumbMap({
    results: [
      { path: '/home/me/ok.jpg', ok: { mime: 'image/jpeg', content: 'T0s=', width: 128, height: 128 } },
      { path: '/home/me/busy.jpg', error: { code: 'busy', message: 'Thumbnail decoding is busy.' } },
      { path: '/home/me/missing.jpg', error: { code: 'not_found', message: 'File not found.' } },
    ],
  });
  assert.equal(mixed['/home/me/ok.jpg'].content, 'T0s=');
  assert.equal(mixed['/home/me/busy.jpg'].error.code, 'busy');
  assert.equal(isRetryableThumbError(mixed['/home/me/busy.jpg'].error), true);
  assert.equal(mixed['/home/me/missing.jpg'].error.code, 'not_found');
  assert.equal(isRetryableThumbError(mixed['/home/me/missing.jpg'].error), false);

  // Empty, malformed, and legacy `{ thumbnails }` shapes produce an empty map without throwing.
  for (const bad of [
    null,
    undefined,
    'not-an-object',
    42,
    [],
    {},
    { results: null },
    { results: 'not-an-array' },
    { results: [] },
    {
      results: [
        null,
        'str',
        {},
        { path: 123, ok: { mime: 'image/png', content: 'AA==' } },
        { path: '', ok: { mime: 'image/png', content: 'AA==' } },
        { path: '/home/me/no-payload.jpg' },
        { path: '/home/me/bad-ok.jpg', ok: 'string' },
        { path: '/home/me/arr-ok.jpg', ok: [] },
      ],
    },
    {
      thumbnails: {
        '/home/me/legacy.jpg': { mime: 'image/jpeg', content: 'TEVH' },
      },
    },
  ]) {
    const out = extractThumbMap(bad);
    assert.equal(Object.getPrototypeOf(out), null);
    assert.deepEqual(Object.keys(out), []);
  }
});

test('createThumbQueue batches at most 32 paths and retries busy errors with exponential backoff', async () => {
  let nextId = 1;
  const timers = new Map();
  const schedule = (fn, ms) => {
    const id = nextId++;
    timers.set(id, { fn, ms });
    return id;
  };
  const cancel = (id) => {
    timers.delete(id);
  };
  const runAllTimers = async () => {
    const snapshot = [...timers.entries()];
    timers.clear();
    for (const [, entry] of snapshot) {
      await entry.fn();
    }
    await Promise.resolve();
    await Promise.resolve();
  };

  // 1. Batching > 32 paths splits cleanly into chunks of at most THUMB_BATCH_MAX (32).
  const batchSizes = [];
  const cache1 = createThumbCache({ maxEntries: 100 });
  let updates1 = 0;
  const q1 = createThumbQueue({
    machine: 'tower',
    link: {
      async call(tool, args) {
        assert.equal(tool, 'fs.thumb');
        batchSizes.push(args.paths.length);
        assert.ok(args.paths.length >= 1 && args.paths.length <= THUMB_BATCH_MAX);
        return {
          results: args.paths.map((p) => ({
            path: p,
            ok: { mime: 'image/jpeg', content: 'QUFB', width: 128, height: 128 },
          })),
        };
      },
    },
    thumbCache: cache1,
    onUpdate: () => {
      updates1 += 1;
    },
    schedule,
    cancel,
  });

  for (let i = 0; i < 40; i += 1) {
    q1.enqueue({ path: `/home/me/pic-${i}.jpg`, mtime: '' });
  }
  await runAllTimers(); // batch 1 (32)
  await runAllTimers(); // batch 2 (8)
  assert.deepEqual(batchSizes, [32, 8]);
  assert.equal(updates1, 2);
  assert.equal(cache1.get(thumbCacheKey('tower', '/home/me/pic-0.jpg', '', 128))?.url, 'data:image/jpeg;base64,QUFB');
  assert.equal(cache1.get(thumbCacheKey('tower', '/home/me/pic-39.jpg', '', 128))?.url, 'data:image/jpeg;base64,QUFB');
  q1.dispose();

  // 2. Per-path busy retries with backoff (350ms -> 700ms -> 1400ms), non-retryable stops immediately.
  const cache2 = createThumbCache({ maxEntries: 50 });
  const attempts = new Map();
  const recordedDelays = [];
  const trackingSchedule = (fn, ms) => {
    recordedDelays.push(ms);
    return schedule(fn, ms);
  };
  let updates2 = 0;
  const q2 = createThumbQueue({
    machine: 'tower',
    link: {
      async call(_tool, args) {
        return {
          results: args.paths.map((p) => {
            const n = (attempts.get(p) || 0) + 1;
            attempts.set(p, n);
            if (p === '/home/me/recovers.jpg') {
              return n === 1
                ? { path: p, error: { code: 'busy', message: 'Busy.' } }
                : { path: p, ok: { mime: 'image/png', content: 'T0s=', width: 128, height: 128 } };
            }
            if (p === '/home/me/always-busy.jpg') {
              return { path: p, error: { code: 'busy', message: 'Still busy.' } };
            }
            return { path: p, error: { code: 'unsupported', message: 'No thumb.' } };
          }),
        };
      },
    },
    thumbCache: cache2,
    onUpdate: () => {
      updates2 += 1;
    },
    debounceMs: 24,
    schedule: trackingSchedule,
    cancel,
  });

  q2.enqueue({ path: '/home/me/recovers.jpg', mtime: 't1' });
  q2.enqueue({ path: '/home/me/always-busy.jpg', mtime: 't1' });
  q2.enqueue({ path: '/home/me/bad.txt', mtime: 't1' });

  // Initial flush.
  await runAllTimers();
  assert.equal(cache2.get(thumbCacheKey('tower', '/home/me/bad.txt', 't1', 128))?.failed, true);
  assert.equal(cache2.get(thumbCacheKey('tower', '/home/me/recovers.jpg', 't1', 128)), null);
  assert.equal(cache2.get(thumbCacheKey('tower', '/home/me/always-busy.jpg', 't1', 128)), null);

  // Re-enqueueing while waiting in retry backoff must not bypass the backoff timer.
  q2.enqueue({ path: '/home/me/recovers.jpg', mtime: 't1' });
  assert.equal(attempts.get('/home/me/recovers.jpg'), 1);

  // Retry 1 (backoff 350ms -> debounce 24ms).
  await runAllTimers(); // retry timers fire and re-enqueue
  await runAllTimers(); // flush runs
  assert.equal(cache2.get(thumbCacheKey('tower', '/home/me/recovers.jpg', 't1', 128))?.url, 'data:image/png;base64,T0s=');
  assert.equal(updates2, 1);
  assert.equal(attempts.get('/home/me/bad.txt'), 1);

  // Retries 2 and 3 for always-busy.jpg until THUMB_MAX_RETRIES exhausted.
  for (let i = 1; i < THUMB_MAX_RETRIES; i += 1) {
    await runAllTimers();
    await runAllTimers();
  }
  assert.equal(attempts.get('/home/me/always-busy.jpg'), 1 + THUMB_MAX_RETRIES);
  assert.equal(cache2.get(thumbCacheKey('tower', '/home/me/always-busy.jpg', 't1', 128))?.failed, true);
  assert.ok(recordedDelays.includes(THUMB_RETRY_BASE_MS));
  assert.ok(recordedDelays.includes(THUMB_RETRY_BASE_MS * 2));
  assert.ok(recordedDelays.includes(THUMB_RETRY_BASE_MS * 4));
  assert.equal(timers.size, 0);
  q2.dispose();

  // 3. Transient capability ensure failure leaves cache unpoisoned so re-enqueuing once
  //    capabilities arrive fetches the thumbnail cleanly.
  const cache3 = createThumbCache({ maxEntries: 20 });
  let capsKnown = false;
  let ensureAttempts = 0;
  let thumbCalls = 0;
  let updates3 = 0;
  const fakeCapsStore = {
    isKnown: () => capsKnown,
    hasTool: (_m, tool) => capsKnown && (tool === 'fs.thumb' || tool === 'fs.tags'),
    fileCapabilities: () => ({ hasThumb: capsKnown, hasAudioThumb: capsKnown }),
    async ensure() {
      ensureAttempts += 1;
      if (ensureAttempts === 1) throw { code: 'unreachable', message: 'Timed out.' };
      capsKnown = true;
      return { status: 'known' };
    },
    async call(tool, args) {
      assert.equal(tool, 'fs.thumb');
      thumbCalls += 1;
      return {
        value: {
          results: args.paths.map((p) => ({
            path: p,
            ok: { mime: 'image/png', content: 'T0s=', width: 128, height: 128 },
          })),
        },
      };
    },
  };
  const q3 = createThumbQueue({
    machine: 'pi',
    link: null,
    capsStore: fakeCapsStore,
    thumbCache: cache3,
    onUpdate: () => {
      updates3 += 1;
    },
    schedule,
    cancel,
  });

  q3.enqueue({ path: '/home/me/late.png', mtime: '' });
  await runAllTimers();
  assert.equal(ensureAttempts, 1);
  assert.equal(thumbCalls, 0);
  assert.equal(cache3.get(thumbCacheKey('pi', '/home/me/late.png', '', 128)), null);

  // Capabilities arrive later; re-enqueueing fetches both picture and song cover thumbnails.
  assert.equal(isThumbEligible({ name: 'late-song.flac', kind: 'file' }, { allowAudio: false }), false);
  assert.equal(isThumbEligible({ name: 'late-song.flac', kind: 'file' }, { allowAudio: true }), true);
  capsKnown = true;
  q3.enqueue({ path: '/home/me/late.png', mtime: '' });
  q3.enqueue({ path: '/home/me/late-song.flac', mtime: '' });
  await runAllTimers();
  assert.equal(thumbCalls, 1);
  assert.equal(updates3, 1);
  assert.equal(cache3.get(thumbCacheKey('pi', '/home/me/late.png', '', 128))?.url, 'data:image/png;base64,T0s=');
  assert.equal(cache3.get(thumbCacheKey('pi', '/home/me/late-song.flac', '', 128))?.url, 'data:image/png;base64,T0s=');
  q3.dispose();

  // Audio covers go through createThumbQueue on nodes with hasAudioThumb, retry busy covers,
  // cache not_found cleanly, and are skipped on older nodes without hasAudioThumb while pictures
  // in the same batch still load.
  const audioCalls = [];
  const audioAttempts = new Map();
  const makeCapsStore = (hasAudioThumb) => ({
    isKnown: () => true,
    hasTool: (_m, tool) => tool === 'fs.thumb' || (hasAudioThumb && tool === 'fs.tags'),
    fileCapabilities: () => ({ hasThumb: true, hasAudioThumb }),
    async ensure() {
      return { status: 'known' };
    },
    async call(tool, args, opts) {
      assert.equal(tool, 'fs.thumb');
      audioCalls.push({ machine: opts.on, paths: [...args.paths] });
      return {
        value: {
          results: args.paths.map((p) => {
            if (p.endsWith('busy.mp3')) {
              const tries = audioAttempts.get(p) || 0;
              audioAttempts.set(p, tries + 1);
              if (tries === 0) {
                return { path: p, error: { code: 'busy', message: 'Reply budget exceeded.' } };
              }
            }
            if (p.endsWith('nocover.flac')) {
              return { path: p, error: { code: 'not_found', message: 'No cover.' } };
            }
            return { path: p, ok: { mime: 'image/jpeg', content: 'Q09WRVI=', width: 128, height: 128 } };
          }),
        },
      };
    },
  });

  const audioCache = createThumbCache();
  let audioUpdates = 0;
  const qAudio = createThumbQueue({
    machine: 'tower',
    link: null,
    capsStore: makeCapsStore(true),
    thumbCache: audioCache,
    onUpdate: () => {
      audioUpdates += 1;
    },
    schedule,
    cancel,
  });
  qAudio.enqueue({ path: '/home/me/Music/track.mp3', mtime: '1' });
  qAudio.enqueue({ path: '/home/me/Music/busy.mp3', mtime: '1' });
  qAudio.enqueue({ path: '/home/me/Music/nocover.flac', mtime: '1' });
  await runAllTimers(); // initial batch
  await runAllTimers(); // retry timer fires -> re-enqueues busy.mp3
  await runAllTimers(); // retry batch completes
  assert.equal(audioCalls.length, 2);
  assert.deepEqual(audioCalls[0].paths, [
    '/home/me/Music/track.mp3',
    '/home/me/Music/busy.mp3',
    '/home/me/Music/nocover.flac',
  ]);
  assert.deepEqual(audioCalls[1].paths, ['/home/me/Music/busy.mp3']);
  assert.equal(audioUpdates, 2);
  assert.equal(
    audioCache.get(thumbCacheKey('tower', '/home/me/Music/track.mp3', '1', 128))?.url,
    'data:image/jpeg;base64,Q09WRVI=',
  );
  assert.equal(
    audioCache.get(thumbCacheKey('tower', '/home/me/Music/busy.mp3', '1', 128))?.url,
    'data:image/jpeg;base64,Q09WRVI=',
  );
  assert.deepEqual(audioCache.get(thumbCacheKey('tower', '/home/me/Music/nocover.flac', '1', 128)), {
    failed: true,
  });
  qAudio.dispose();

  // Pre-audio-thumb machine: skips audio paths even if queued before capabilities arrive, while
  // still fetching picture thumbnails in the same batch.
  let oldKnown = false;
  const oldAudioCalls = [];
  const oldCapsStore = {
    isKnown: () => oldKnown,
    hasTool: (_m, tool) => oldKnown && tool === 'fs.thumb',
    fileCapabilities: () => ({ hasThumb: oldKnown, hasAudioThumb: false }),
    async ensure() {
      oldKnown = true;
      return { status: 'known' };
    },
    async call(tool, args) {
      assert.equal(tool, 'fs.thumb');
      oldAudioCalls.push([...args.paths]);
      return {
        value: {
          results: args.paths.map((p) => ({
            path: p,
            ok: { mime: 'image/jpeg', content: 'UEhPVE8=', width: 128, height: 128 },
          })),
        },
      };
    },
  };
  const oldCache = createThumbCache();
  const qOldAudio = createThumbQueue({
    machine: 'pretags',
    link: null,
    capsStore: oldCapsStore,
    thumbCache: oldCache,
    schedule,
    cancel,
  });
  qOldAudio.enqueue({ path: '/home/me/Music/track.mp3', mtime: '1' });
  qOldAudio.enqueue({ path: '/home/me/Photos/pic.jpg', mtime: '1' });
  await runAllTimers();
  assert.deepEqual(oldAudioCalls, [['/home/me/Photos/pic.jpg']]);
  assert.equal(oldCache.get(thumbCacheKey('pretags', '/home/me/Music/track.mp3', '1', 128)), null);
  assert.equal(
    oldCache.get(thumbCacheKey('pretags', '/home/me/Photos/pic.jpg', '1', 128))?.url,
    'data:image/jpeg;base64,UEhPVE8=',
  );
  qOldAudio.dispose();
});

test('createThumbQueue pauses queued flushes and retry backoff while hidden and resumes once visible', async () => {
  const timers = new Map();
  let nextTimerId = 1;
  const schedule = (fn, ms) => {
    const id = nextTimerId++;
    timers.set(id, { fn, ms });
    return id;
  };
  const cancel = (id) => {
    timers.delete(id);
  };
  const runAllTimers = async () => {
    while (timers.size > 0) {
      const [id, item] = timers.entries().next().value;
      timers.delete(id);
      await item.fn();
    }
  };

  let visible = false;
  let calls = 0;
  let failBusy = true;
  const link = {
    async call(tool, args) {
      assert.equal(tool, 'fs.thumb');
      calls += 1;
      if (failBusy) {
        return { results: args.paths.map((p) => ({ path: p, error: { code: 'busy', message: 'busy' } })) };
      }
      return {
        results: args.paths.map((p) => ({
          path: p,
          ok: { mime: 'image/jpeg', content: 'UEhPVE8=', width: 128, height: 128 },
        })),
      };
    },
  };

  const cache = createThumbCache();
  const q = createThumbQueue({
    machine: 'nas',
    link,
    thumbCache: cache,
    isVisible: () => visible,
    schedule,
    cancel,
  });

  // Enqueueing while hidden schedules no timer and makes zero calls.
  q.enqueue({ path: '/home/me/Photos/a.jpg', mtime: '1' });
  assert.equal(timers.size, 0);
  await runAllTimers();
  assert.equal(calls, 0);

  // Becoming visible flushes once; busy error schedules a retry timer.
  visible = true;
  q.onVisibilityChange();
  assert.equal(timers.size, 1);
  // Run only the flush timer so retry timer is pending.
  const [firstId, firstTimer] = timers.entries().next().value;
  timers.delete(firstId);
  await firstTimer.fn();
  assert.equal(calls, 1);
  assert.equal(timers.size, 1, 'busy item should schedule backoff retry while visible');

  // Hiding cancels the pending retry timer immediately and re-queues the item without firing.
  visible = false;
  q.onVisibilityChange();
  assert.equal(timers.size, 0, 'hidden queue must cancel pending backoff retry timers');
  await runAllTimers();
  assert.equal(calls, 1);

  // Returning to visible flushes the deferred retry item once and populates cache.
  failBusy = false;
  visible = true;
  q.onVisibilityChange();
  await runAllTimers();
  assert.equal(calls, 2);
  assert.equal(cache.get(thumbCacheKey('nas', '/home/me/Photos/a.jpg', '1', 128))?.url, 'data:image/jpeg;base64,UEhPVE8=');
  q.dispose();
});

test('createVirtualListLayout handles mixed variable heights, estimate corrections without jump, and scroll-to-index', () => {
  const layout = createVirtualListLayout({ estimatedHeight: 60 });
  const keys = Array.from({ length: 5000 }, (_, i) => `/dir/item_${String(i).padStart(4, '0')}.txt`);
  layout.syncItems(keys);

  assert.equal(layout.getCount(), 5000);
  assert.equal(layout.getTotalHeight(), 5000 * 60);
  assert.equal(layout.getOffset(0), 0);
  assert.equal(layout.getOffset(100), 6000);
  assert.equal(layout.findIndexAtOffset(0), 0);
  assert.equal(layout.findIndexAtOffset(6000), 100);
  assert.equal(layout.findIndexAtOffset(6059), 100);
  assert.equal(layout.findIndexAtOffset(6060), 101);

  // Top window covers viewport + overscan and preserves exact total height.
  const topWin = layout.computeWindow({ scrollOffset: 0, viewportHeight: 720, overscanPx: 600 });
  assert.equal(topWin.start, 0);
  assert.equal(topWin.topPad, 0);
  assert.equal(topWin.end, Math.ceil((720 + 600) / 60));
  assert.equal(
    topWin.topPad + (layout.getOffset(topWin.end) - layout.getOffset(topWin.start)) + topWin.bottomPad,
    layout.getTotalHeight(),
  );

  // Hysteresis keeps the mounted window unchanged while the viewport still has >= minBufferPx
  // of mounted rows above and below, preventing redundant re-renders on small scroll steps.
  const smallStep = layout.computeWindow({
    scrollOffset: 120,
    viewportHeight: 720,
    overscanPx: 600,
    minBufferPx: 280,
    prevWindow: topWin,
  });
  assert.equal(smallStep.start, topWin.start);
  assert.equal(smallStep.end, topWin.end);

  // Scrolling past the hysteresis buffer shifts the window cleanly.
  const scrolledWin = layout.computeWindow({
    scrollOffset: 12_000, // row 200
    viewportHeight: 720,
    overscanPx: 600,
    minBufferPx: 280,
    prevWindow: topWin,
  });
  assert.equal(scrolledWin.start, 190);
  assert.equal(scrolledWin.end, 222);

  // Estimate correction ABOVE the viewport anchor returns exact positive scrollDelta so visible
  // content does not jump when 1-line estimates wrap to 2-line rows.
  const scrollOff = 12_000; // anchor is row 200 (offset 12,000)
  const aboveUpdates = [
    { index: 195, key: keys[195], height: 82 }, // +22
    { index: 199, key: keys[199], height: 80 }, // +20 (partially/strictly above anchor 200)
    { index: 205, key: keys[205], height: 84 }, // +24 (below anchor: must NOT shift scrollTop)
  ];
  const anchorBefore = layout.getOffset(200);
  const resAbove = layout.applyMeasurements({
    updates: aboveUpdates,
    scrollOffset: scrollOff,
  });
  assert.equal(resAbove.changed, true);
  assert.equal(resAbove.anchorIndex, 200);
  assert.equal(resAbove.scrollDelta, 42);
  assert.equal(layout.getOffset(200) - (scrollOff + resAbove.scrollDelta), anchorBefore - scrollOff);
  assert.equal(layout.getTotalHeight(), 5000 * 60 + 22 + 20 + 24);

  // Corrections strictly AT or BELOW the anchor produce zero scrollDelta.
  const resBelow = layout.applyMeasurements({
    updates: [
      { index: 200, key: keys[200], height: 86 },
      { index: 201, key: keys[201], height: 86 },
    ],
    scrollOffset: scrollOff + resAbove.scrollDelta,
  });
  assert.equal(resBelow.changed, true);
  assert.equal(resBelow.scrollDelta, 0);

  // At scrollOffset = 0, corrections never push scrollTop away from the top of the list.
  const resAtTop = layout.applyMeasurements({
    updates: [{ index: 0, key: keys[0], height: 84 }],
    scrollOffset: 0,
  });
  assert.equal(resAtTop.scrollDelta, 0);

  // Updating the live one-line estimatedHeight shifts unmeasured rows above the anchor and
  // reports the exact combined scrollDelta to keep the anchor row pinned.
  const midOff = layout.getOffset(200) + 10; // anchor becomes row 201
  const old201 = layout.getOffset(201);
  const resEst = layout.applyMeasurements({
    estimatedHeight: 64, // +4 on all unmeasured rows
    scrollOffset: midOff,
  });
  assert.equal(resEst.anchorIndex, 201);
  // Rows 0..200 have 4 measured rows (0, 195, 199, 200), so 197 unmeasured rows grew by +4 = +788.
  assert.equal(resEst.scrollDelta, 197 * 4);
  assert.equal(layout.getOffset(201) - old201, 197 * 4);

  // Append-only paging preserves measured heights and extends offsets incrementally.
  const extendedKeys = [
    ...keys,
    ...Array.from({ length: 500 }, (_, i) => `/dir/item_${String(5000 + i).padStart(4, '0')}.txt`),
  ];
  const prevTotal = layout.getTotalHeight();
  layout.syncItems(extendedKeys);
  assert.equal(layout.getCount(), 5500);
  assert.equal(layout.getHeight(195), 82);
  assert.equal(layout.getTotalHeight(), prevTotal + 500 * 64);

  // Dynamic viewport height + bottom inset (e.g. mini player bar appearing): window covers full
  // viewport and computeScrollTargetForIndex keeps the target above the bottom inset.
  const targetIdx = 4999;
  const targetTop = layout.computeScrollTargetForIndex(targetIdx, {
    listTop: 40,
    viewportHeight: 844,
    topInset: 44,
    bottomInset: 96,
    align: 'end',
  });
  const itemBottom = 40 + layout.getOffset(targetIdx) + layout.getHeight(targetIdx);
  assert.equal(itemBottom - targetTop, 844 - 96);

  // `onlyIfOffscreen` returns null when the item is already comfortably inside the unobstructed
  // viewport, and returns a scroll target if hidden behind the bottom inset.
  const idx300 = 300;
  const top300 = 40 + layout.getOffset(idx300);
  assert.equal(
    layout.computeScrollTargetForIndex(idx300, {
      listTop: 40,
      viewportHeight: 844,
      topInset: 44,
      bottomInset: 96,
      currentScrollTop: top300 - 200,
      onlyIfOffscreen: true,
    }),
    null,
  );
  assert.notEqual(
    layout.computeScrollTargetForIndex(idx300, {
      listTop: 40,
      viewportHeight: 844,
      topInset: 44,
      bottomInset: 96,
      currentScrollTop: top300 - 760, // sits under the 96px bottom inset
      onlyIfOffscreen: true,
    }),
    null,
  );

  // Resetting measurements (e.g. after width change) clears cached heights and adopts live estimate.
  layout.resetMeasurements({ estimatedHeight: 58 });
  assert.equal(layout.isMeasured(195), false);
  assert.equal(layout.getEstimatedHeight(), 58);
  assert.equal(layout.getTotalHeight(), 5500 * 58);
});

test('computeVirtualWindow and computeVirtualGridWindow bound rendered slices and preserve geometry', () => {
  assert.deepEqual(
    computeVirtualWindow({ total: 0, rowHeight: 56, scrollOffset: 0, viewportHeight: 720 }),
    { start: 0, end: 0, topPad: 0, bottomPad: 0 },
  );

  const mid = computeVirtualWindow({
    total: 5000,
    rowHeight: 56,
    scrollOffset: 140_000,
    viewportHeight: 720,
    overscan: 20,
  });
  assert.equal(mid.start, 2480);
  assert.equal(mid.topPad + (mid.end - mid.start) * 56 + mid.bottomPad, 5000 * 56);

  // Grid: 5,000 square cells in 4 columns, 124px cells + 3px gap.
  const gridTop = computeVirtualGridWindow({
    total: 5000,
    columns: 4,
    rowHeight: 124,
    rowGap: 3,
    scrollOffset: 0,
    viewportHeight: 720,
    overscanRows: 4,
  });
  assert.equal(gridTop.startIndex, 0);
  assert.equal(gridTop.topPad, 0);
  assert.equal(gridTop.endIndex % 4, 0);
  assert.ok(gridTop.endIndex > 20 && gridTop.endIndex < 80);

  const gridMid = computeVirtualGridWindow({
    total: 5000,
    columns: 4,
    rowHeight: 124,
    rowGap: 3,
    scrollOffset: 50_000,
    viewportHeight: 720,
    overscanRows: 4,
  });
  assert.equal(gridMid.startIndex % 4, 0);
  assert.equal(gridMid.endIndex % 4, 0);
  assert.ok(gridMid.topPad > 0 && gridMid.bottomPad > 0);
  // Top spacer + rowGap + mounted rows + rowGap + bottom spacer === totalHeight.
  const mountedRows = gridMid.endRow - gridMid.startRow;
  const mountedH = mountedRows * 124 + Math.max(0, mountedRows - 1) * 3;
  assert.equal(gridMid.topPad + 3 + mountedH + 3 + gridMid.bottomPad, gridMid.totalHeight);

  const gridEnd = computeVirtualGridWindow({
    total: 5000,
    columns: 4,
    rowHeight: 124,
    rowGap: 3,
    scrollOffset: gridMid.totalHeight,
    viewportHeight: 720,
    overscanRows: 4,
  });
  assert.equal(gridEnd.endIndex, 5000);
  assert.equal(gridEnd.bottomPad, 0);
});
