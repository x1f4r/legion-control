// Hash routing and one-time #token= extraction.

export const MACHINE_TABS = ['overview', 'run', 'files', 'jobs', 'services', 'projects', 'power'];

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
    if (segments[2] === 'jobs' && segments[3]) {
      return { section: 'machines', machine, tab: 'jobs', jobId: segments[3] };
    }
    const tab = MACHINE_TABS.includes(segments[2]) ? segments[2] : 'overview';
    return { section: 'machines', machine, tab, jobId: null };
  }

  if (segments[0] === 'jobs') {
    return {
      section: 'jobs',
      machine: segments[1] || null,
      jobId: segments[2] || null,
    };
  }

  if (segments[0] === 'tools') {
    return {
      section: 'tools',
      tool: segments[1] || null,
    };
  }

  if (segments[0] === 'device') {
    return { section: 'device' };
  }

  return { section: 'machines', machine: null, tab: 'overview', jobId: null };
}

export function routeHref(route) {
  const enc = (s) => encodeURIComponent(String(s));
  if (!route || route.section === 'machines') {
    if (!route?.machine) return '#/machines';
    if (route.jobId) return `#/machines/${enc(route.machine)}/jobs/${enc(route.jobId)}`;
    const tab = route.tab && route.tab !== 'overview' ? `/${enc(route.tab)}` : '';
    return `#/machines/${enc(route.machine)}${tab}`;
  }
  if (route.section === 'jobs') {
    if (route.machine && route.jobId) {
      return `#/jobs/${enc(route.machine)}/${enc(route.jobId)}`;
    }
    return '#/jobs';
  }
  if (route.section === 'tools') {
    return route.tool ? `#/tools/${enc(route.tool)}` : '#/tools';
  }
  if (route.section === 'device') return '#/device';
  return '#/machines';
}
