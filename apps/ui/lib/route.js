// Hash routing for the Legion interface, Files path encoding, and one-time #token= extraction.

export const ROOT_SECTIONS = ['machines', 'files', 'activity', 'actions'];
export const MACHINE_PARTS = ['overview', 'files', 'power', 'more'];
export const MACHINE_TABS = ['overview', 'files', 'power', 'command', 'services', 'projects', 'activity', 'more'];
export const WIDE_MACHINE_TABS = ['overview', 'files', 'power', 'command', 'services', 'projects', 'activity'];
export const SETTINGS_PAGES = ['device', 'connections', 'appearance', 'refreshing', 'about'];

function safeDecode(value) {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/**
 * Encodes a file or directory path for `#/files/<machine>/<encoded>` without losing special
 * characters (`#`, `?`, `&`, `%`, spaces, Unicode), Windows drive/UNC paths, or root `/`.
 */
export function encodeFilePath(filePath) {
  if (typeof filePath !== 'string' || !filePath) return '~';
  if (filePath === '/') return '/';
  if (filePath.startsWith('/')) {
    const trimmed = filePath.replace(/\/+$/, '') || '/';
    if (trimmed === '/') return '/';
    const segments = trimmed.slice(1).split('/').map((seg) => encodeURIComponent(seg));
    return `/${segments.join('/')}`;
  }
  if (filePath === '~') return '~';
  if (filePath.startsWith('~/')) {
    const trimmed = filePath.replace(/\/+$/, '');
    const rest = trimmed.slice(2);
    if (!rest) return '~';
    const segments = rest.split('/').map((seg) => encodeURIComponent(seg));
    return `~/${segments.join('/')}`;
  }
  // Windows drive paths (e.g. C:\Users\me\Photos, C:\), UNC paths, or relative paths.
  return encodeURIComponent(filePath);
}

/**
 * Decodes the path tail from `#/files/<machine>/<encoded>` back into the exact path string.
 */
export function decodeFilePath(encoded) {
  if (typeof encoded !== 'string' || !encoded) return '~';
  if (encoded === '/') return '/';
  if (encoded.startsWith('/')) {
    const parts = encoded.slice(1).split('/').map(safeDecode);
    return `/${parts.join('/')}`;
  }
  if (encoded === '~') return '~';
  if (encoded.startsWith('~/')) {
    const parts = encoded.slice(2).split('/').map(safeDecode);
    return `~/${parts.join('/')}`;
  }
  return safeDecode(encoded);
}

function parseViewerParams(params, fallbackMachine) {
  const vp = params.get('vp') || params.get('view');
  if (!vp) return null;
  const vm = params.get('vm') || fallbackMachine || null;
  if (!vm) return null;
  return { machine: vm, path: vp };
}

function appendViewerParams(params, viewer, defaultMachine) {
  if (!viewer || !viewer.path) return;
  if (viewer.machine && viewer.machine !== defaultMachine) {
    params.set('vm', viewer.machine);
    params.set('vp', viewer.path);
  } else {
    params.set('view', viewer.path);
  }
}

export function extractHashToken(hash) {
  if (!hash || typeof hash !== 'string') return { token: null, cleanHash: '' };
  const raw = hash.startsWith('#') ? hash.slice(1) : hash;
  if (raw.startsWith('token=')) {
    const amp = raw.indexOf('&');
    const encoded = amp === -1 ? raw.slice(6) : raw.slice(6, amp);
    let token = null;
    try {
      token = decodeURIComponent(encoded).trim() || null;
    } catch {
      token = encoded.trim() || null;
    }
    return { token, cleanHash: '' };
  }
  const match = raw.match(/(?:^|[?&])token=([^&]+)/);
  if (match && !raw.startsWith('/')) {
    let token = null;
    try {
      token = decodeURIComponent(match[1]).trim() || null;
    } catch {
      token = match[1].trim() || null;
    }
    return { token, cleanHash: '' };
  }
  return { token: null, cleanHash: hash };
}

function parseFilesRoute(pathPart, queryStr) {
  const params = new URLSearchParams(queryStr || '');
  const trimmed = pathPart.replace(/^\/+/, '');
  const afterFiles = trimmed.slice('files'.length);

  if (!afterFiles || afterFiles === '/') {
    return {
      section: 'files',
      view: 'home',
      machine: null,
      path: null,
      syncId: null,
      query: null,
      viewer: parseViewerParams(params, null),
    };
  }

  const rest = afterFiles.startsWith('/') ? afterFiles.slice(1) : afterFiles;
  const slashIdx = rest.indexOf('/');
  const headEnc = slashIdx === -1 ? rest : rest.slice(0, slashIdx);
  const tail = slashIdx === -1 ? '' : rest.slice(slashIdx + 1);
  const head = safeDecode(headEnc);

  if (head === '@favourites') {
    return {
      section: 'files',
      view: 'favourites',
      machine: null,
      path: null,
      syncId: null,
      query: null,
      viewer: parseViewerParams(params, null),
    };
  }

  if (head === '@recent') {
    return {
      section: 'files',
      view: 'recent',
      machine: null,
      path: null,
      syncId: null,
      query: null,
      viewer: parseViewerParams(params, null),
    };
  }

  if (head === '@search') {
    const kindsRaw = params.get('kinds') || '';
    const kinds = kindsRaw
      ? kindsRaw
          .split(',')
          .map((k) => k.trim())
          .filter(Boolean)
      : [];
    const searchMachine = params.get('m') || null;
    const searchPath = params.get('p') || null;
    return {
      section: 'files',
      view: 'search',
      machine: searchMachine,
      path: searchPath,
      syncId: null,
      query: {
        q: params.get('q') || '',
        kinds,
        size: params.get('size') || '',
        changed: params.get('changed') || '',
      },
      viewer: parseViewerParams(params, searchMachine),
    };
  }

  if (head === '@sync') {
    const syncId = tail ? safeDecode(tail.split('/')[0]) : null;
    return {
      section: 'files',
      view: syncId ? 'sync' : 'home',
      machine: null,
      path: null,
      syncId,
      query: null,
      viewer: parseViewerParams(params, null),
    };
  }

  const machine = head || null;
  const filePath = tail ? decodeFilePath(tail) : '~';
  return {
    section: 'files',
    view: machine ? 'folder' : 'home',
    machine,
    path: machine ? filePath : null,
    syncId: null,
    query: null,
    viewer: parseViewerParams(params, machine),
  };
}

export function parseRoute(hash) {
  const raw = (hash || '').replace(/^#/, '');
  const qIdx = raw.indexOf('?');
  const path = qIdx === -1 ? raw : raw.slice(0, qIdx);
  const queryStr = qIdx === -1 ? '' : raw.slice(qIdx + 1);

  if (path === '/files' || path.startsWith('/files/') || path === 'files' || path.startsWith('files/')) {
    return parseFilesRoute(path, queryStr);
  }

  const segments = path
    .split('/')
    .filter(Boolean)
    .map(safeDecode);

  if (segments.length === 0 || segments[0] === 'machines') {
    const machine = segments[1] || null;
    if (!machine) return { section: 'machines', machine: null, tab: 'overview', jobId: null };
    // Map legacy /run and /jobs machine tabs into the v2 names.
    const rawTab = segments[2] === 'run' ? 'command' : segments[2] === 'jobs' ? 'activity' : segments[2];
    if (rawTab === 'activity' && segments[3]) {
      return { section: 'machines', machine, tab: 'activity', jobId: segments[3] };
    }
    const tab = MACHINE_TABS.includes(rawTab) ? rawTab : 'overview';
    return { section: 'machines', machine, tab, jobId: null };
  }

  if (segments[0] === 'activity' || segments[0] === 'jobs') {
    return {
      section: 'activity',
      machine: segments[1] || null,
      jobId: segments[2] || null,
    };
  }

  if (segments[0] === 'actions') {
    if (segments[1] === 'all') {
      return {
        section: 'actions',
        allTools: true,
        tool: segments[2] || null,
        action: null,
      };
    }
    return {
      section: 'actions',
      allTools: false,
      tool: null,
      action: segments[1] || null,
    };
  }

  if (segments[0] === 'tools') {
    return {
      section: 'actions',
      allTools: true,
      tool: segments[1] || null,
      action: null,
    };
  }

  if (segments[0] === 'settings') {
    const page = SETTINGS_PAGES.includes(segments[1]) ? segments[1] : null;
    return {
      section: 'settings',
      page,
      sub: page ? segments[2] || null : null,
      item: page && segments[2] ? segments[3] || null : null,
    };
  }

  if (segments[0] === 'device') {
    return {
      section: 'settings',
      page: 'device',
      sub: null,
      item: null,
    };
  }

  return { section: 'machines', machine: null, tab: 'overview', jobId: null };
}

export function routeHref(route) {
  const enc = (s) => encodeURIComponent(String(s));
  if (!route || route.section === 'machines') {
    if (!route?.machine) return '#/machines';
    if (route.jobId) return `#/machines/${enc(route.machine)}/activity/${enc(route.jobId)}`;
    const tab = route.tab && route.tab !== 'overview' ? `/${enc(route.tab)}` : '';
    return `#/machines/${enc(route.machine)}${tab}`;
  }
  if (route.section === 'files') {
    const params = new URLSearchParams();
    let base = '#/files';
    if (route.view === 'favourites') {
      base = '#/files/@favourites';
      appendViewerParams(params, route.viewer, null);
    } else if (route.view === 'recent') {
      base = '#/files/@recent';
      appendViewerParams(params, route.viewer, null);
    } else if (route.view === 'sync' && route.syncId) {
      base = `#/files/@sync/${enc(route.syncId)}`;
      appendViewerParams(params, route.viewer, null);
    } else if (route.view === 'search') {
      base = '#/files/@search';
      if (route.machine) params.set('m', route.machine);
      if (route.path) params.set('p', route.path);
      if (route.query?.q) params.set('q', route.query.q);
      if (Array.isArray(route.query?.kinds) && route.query.kinds.length > 0) {
        params.set('kinds', route.query.kinds.join(','));
      }
      if (route.query?.size) params.set('size', route.query.size);
      if (route.query?.changed) params.set('changed', route.query.changed);
      appendViewerParams(params, route.viewer, route.machine || null);
    } else if ((route.view === 'folder' || route.machine) && route.machine) {
      const encoded = encodeFilePath(route.path || '~');
      base = `#/files/${enc(route.machine)}/${encoded}`;
      appendViewerParams(params, route.viewer, route.machine);
    } else {
      appendViewerParams(params, route.viewer, null);
    }
    const qs = params.toString();
    return qs ? `${base}?${qs}` : base;
  }
  if (route.section === 'activity' || route.section === 'jobs') {
    if (route.machine && route.jobId) {
      return `#/activity/${enc(route.machine)}/${enc(route.jobId)}`;
    }
    return '#/activity';
  }
  if (route.section === 'actions') {
    if (route.allTools) {
      return route.tool ? `#/actions/all/${enc(route.tool)}` : '#/actions/all';
    }
    return route.action ? `#/actions/${enc(route.action)}` : '#/actions';
  }
  if (route.section === 'tools') {
    return route.tool ? `#/actions/all/${enc(route.tool)}` : '#/actions/all';
  }
  if (route.section === 'settings') {
    if (!route.page) return '#/settings';
    if (route.sub && route.item) {
      return `#/settings/${enc(route.page)}/${enc(route.sub)}/${enc(route.item)}`;
    }
    if (route.sub) {
      return `#/settings/${enc(route.page)}/${enc(route.sub)}`;
    }
    return `#/settings/${enc(route.page)}`;
  }
  return '#/machines';
}
