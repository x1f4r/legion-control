import assert from 'node:assert/strict';
import test from 'node:test';
import {
  baseFileName,
  buildTransferDestination,
  canOpenExternally,
  categoryFromName,
  changedPresetCutoff,
  createThumbQueue,
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
    hasTool: (_m, tool) => capsKnown && tool === 'fs.thumb',
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

  // Capabilities arrive later; re-enqueueing fetches the thumbnail.
  capsKnown = true;
  q3.enqueue({ path: '/home/me/late.png', mtime: '' });
  await runAllTimers();
  assert.equal(thumbCalls, 1);
  assert.equal(updates3, 1);
  assert.equal(cache3.get(thumbCacheKey('pi', '/home/me/late.png', '', 128))?.url, 'data:image/png;base64,T0s=');
  q3.dispose();
});
