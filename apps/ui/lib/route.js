// Hash routing for the second-pass structure and one-time #token= extraction.

export const ROOT_SECTIONS = ['machines', 'activity', 'actions'];
export const MACHINE_PARTS = ['overview', 'files', 'power', 'more'];
export const MACHINE_TABS = ['overview', 'files', 'power', 'command', 'services', 'projects', 'activity', 'more'];
export const WIDE_MACHINE_TABS = ['overview', 'files', 'power', 'command', 'services', 'projects', 'activity'];
export const SETTINGS_PAGES = ['device', 'connections', 'appearance', 'refreshing', 'about'];

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

export function parseRoute(hash) {
  const raw = (hash || '').replace(/^#/, '');
  const path = raw.split('?')[0];
  const segments = path
    .split('/')
    .filter(Boolean)
    .map((part) => {
      try {
        return decodeURIComponent(part);
      } catch {
        return part;
      }
    });

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
