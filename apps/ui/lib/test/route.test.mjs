import assert from 'node:assert/strict';
import test from 'node:test';
import {
  decodeFilePath,
  encodeFilePath,
  extractHashToken,
  parseRoute,
  ROOT_SECTIONS,
  routeHref,
} from '../route.js';

test('ROOT_SECTIONS includes the four top-level tabs in order', () => {
  assert.deepEqual(ROOT_SECTIONS, ['machines', 'files', 'activity', 'actions']);
});

test('extractHashToken reads #token= once and clears it', () => {
  assert.deepEqual(extractHashToken('#token=lgn_abc123'), {
    token: 'lgn_abc123',
    cleanHash: '',
  });
  assert.deepEqual(extractHashToken('#token=lgn_a%2Bb&next=1'), {
    token: 'lgn_a+b',
    cleanHash: '',
  });
  assert.deepEqual(extractHashToken('#/machines/tower'), {
    token: null,
    cleanHash: '#/machines/tower',
  });
});

test('parseRoute and routeHref round-trip all v2 views', () => {
  const cases = [
    ['', { section: 'machines', machine: null, tab: 'overview', jobId: null }, '#/machines'],
    ['#/machines', { section: 'machines', machine: null, tab: 'overview', jobId: null }, '#/machines'],
    [
      '#/machines/tower',
      { section: 'machines', machine: 'tower', tab: 'overview', jobId: null },
      '#/machines/tower',
    ],
    [
      '#/machines/tower/command',
      { section: 'machines', machine: 'tower', tab: 'command', jobId: null },
      '#/machines/tower/command',
    ],
    [
      '#/machines/tower/more',
      { section: 'machines', machine: 'tower', tab: 'more', jobId: null },
      '#/machines/tower/more',
    ],
    [
      '#/machines/tower/activity/abc-1',
      { section: 'machines', machine: 'tower', tab: 'activity', jobId: 'abc-1' },
      '#/machines/tower/activity/abc-1',
    ],
    ['#/activity', { section: 'activity', machine: null, jobId: null }, '#/activity'],
    [
      '#/activity/tower/abc-1',
      { section: 'activity', machine: 'tower', jobId: 'abc-1' },
      '#/activity/tower/abc-1',
    ],
    ['#/actions', { section: 'actions', allTools: false, tool: null, action: null }, '#/actions'],
    [
      '#/actions/sleep',
      { section: 'actions', allTools: false, tool: null, action: 'sleep' },
      '#/actions/sleep',
    ],
    [
      '#/actions/all',
      { section: 'actions', allTools: true, tool: null, action: null },
      '#/actions/all',
    ],
    [
      '#/actions/all/power.restart',
      { section: 'actions', allTools: true, tool: 'power.restart', action: null },
      '#/actions/all/power.restart',
    ],
    ['#/settings', { section: 'settings', page: null, sub: null, item: null }, '#/settings'],
    [
      '#/settings/appearance',
      { section: 'settings', page: 'appearance', sub: null, item: null },
      '#/settings/appearance',
    ],
    [
      '#/settings/connections/invite',
      { section: 'settings', page: 'connections', sub: 'invite', item: null },
      '#/settings/connections/invite',
    ],
    [
      '#/settings/connections/node/tower',
      { section: 'settings', page: 'connections', sub: 'node', item: 'tower' },
      '#/settings/connections/node/tower',
    ],
  ];
  for (const [hash, expected, href] of cases) {
    const parsed = parseRoute(hash);
    assert.deepEqual(parsed, expected, hash);
    assert.equal(routeHref(parsed), href, hash);
  }
});

test('Files routes encode and decode every path shape safely', () => {
  const paths = [
    '/',
    '~',
    '~/Videos/Holiday',
    '/home/me',
    '/home/me/Videos/Holiday',
    '/home/me/My Photos/Summer 2026',
    '/home/me/weird #hash?q=1&b=2 100%+done/file [1] (copy).jpg',
    '/home/me/back\\slash/café-日本語',
    'C:\\',
    'C:\\Users\\me\\Photos\\Holiday #1?v=2',
    '\\\\nas\\media\\Films',
  ];

  for (const p of paths) {
    const enc = encodeFilePath(p);
    assert.equal(decodeFilePath(enc), p, `encode/decode round-trip failed for ${p}`);
    const href = routeHref({ section: 'files', view: 'folder', machine: 'tower', path: p });
    const parsed = parseRoute(href);
    assert.equal(parsed.section, 'files');
    assert.equal(parsed.view, 'folder');
    assert.equal(parsed.machine, 'tower');
    assert.equal(parsed.path, p, `routeHref/parseRoute failed for ${p} (${href})`);
  }

  // Root `/` has an explicit, unambiguous URL.
  const rootHref = routeHref({ section: 'files', view: 'folder', machine: 'tower', path: '/' });
  assert.equal(rootHref, '#/files/tower//');
  assert.equal(parseRoute(rootHref).path, '/');

  // Viewer query parameters round-trip cleanly on folder, home, and search routes.
  const folderViewerHref = routeHref({
    section: 'files',
    view: 'folder',
    machine: 'tower',
    path: '/home/me/Videos/Holiday',
    viewer: { machine: 'tower', path: '/home/me/Videos/Holiday/IMG #1?.jpg' },
  });
  const parsedFolderViewer = parseRoute(folderViewerHref);
  assert.equal(parsedFolderViewer.path, '/home/me/Videos/Holiday');
  assert.deepEqual(parsedFolderViewer.viewer, {
    machine: 'tower',
    path: '/home/me/Videos/Holiday/IMG #1?.jpg',
  });

  const homeViewerHref = routeHref({
    section: 'files',
    view: 'home',
    viewer: { machine: 'pi', path: '/home/me/Documents/Passport.pdf' },
  });
  assert.deepEqual(parseRoute(homeViewerHref).viewer, {
    machine: 'pi',
    path: '/home/me/Documents/Passport.pdf',
  });

  // Files special views: home, favourites, recent, sync, search.
  assert.equal(parseRoute('#/files').view, 'home');
  assert.equal(routeHref({ section: 'files', view: 'home' }), '#/files');

  assert.equal(parseRoute('#/files/@favourites').view, 'favourites');
  assert.equal(routeHref({ section: 'files', view: 'favourites' }), '#/files/@favourites');

  assert.equal(parseRoute('#/files/@recent').view, 'recent');
  assert.equal(routeHref({ section: 'files', view: 'recent' }), '#/files/@recent');

  const syncHref = routeHref({ section: 'files', view: 'sync', syncId: 'camera-sync' });
  assert.equal(syncHref, '#/files/@sync/camera-sync');
  assert.equal(parseRoute(syncHref).syncId, 'camera-sync');

  const searchHref = routeHref({
    section: 'files',
    view: 'search',
    machine: 'tower',
    path: '/home/me/Videos/Holiday',
    query: { q: 'arrival', kinds: ['video', 'image'], size: 'large', changed: 'week' },
  });
  const parsedSearch = parseRoute(searchHref);
  assert.equal(parsedSearch.view, 'search');
  assert.equal(parsedSearch.machine, 'tower');
  assert.equal(parsedSearch.path, '/home/me/Videos/Holiday');
  assert.deepEqual(parsedSearch.query, {
    q: 'arrival',
    kinds: ['video', 'image'],
    size: 'large',
    changed: 'week',
  });
});

test('parseRoute accepts legacy aliases and falls back safely', () => {
  assert.deepEqual(parseRoute('#/jobs/tower/j-1'), {
    section: 'activity',
    machine: 'tower',
    jobId: 'j-1',
  });
  assert.deepEqual(parseRoute('#/tools/meta.version'), {
    section: 'actions',
    allTools: true,
    tool: 'meta.version',
    action: null,
  });
  assert.deepEqual(parseRoute('#/device'), {
    section: 'settings',
    page: 'device',
    sub: null,
    item: null,
  });
  assert.deepEqual(parseRoute('#/machines/tower/run'), {
    section: 'machines',
    machine: 'tower',
    tab: 'command',
    jobId: null,
  });
  assert.deepEqual(parseRoute('#/machines/pi/nonexistent'), {
    section: 'machines',
    machine: 'pi',
    tab: 'overview',
    jobId: null,
  });
  assert.deepEqual(parseRoute('#/unknown'), {
    section: 'machines',
    machine: null,
    tab: 'overview',
    jobId: null,
  });
});
