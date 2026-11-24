// Legion interface entry point: startup, hash routing, polling, confirmation sheet, and shell.

import { html, render, useCallback, useEffect, useMemo, useRef, useState } from './vendor/preact-htm.js';
import { Icon, IconSprite } from './icons.js';
import { createCache } from './lib/cache.js';
import { firstSentence, formatRelativeTime } from './lib/format.js';
import { fanOutEntries, isFanOut } from './lib/result.js';
import { extractHashToken, parseRoute, routeHref } from './lib/route.js';
import { describePowerConfirm } from './lib/schema.js';
import { createLink } from './link/index.js';
import { Notice, Sheet } from './views/common.js';
import { DeviceView } from './views/device.js';
import { JobsView } from './views/jobs.js';
import { MachinesView } from './views/machines.js';
import { StartView } from './views/start.js';
import { ToolsView } from './views/tools.js';

const THEME_KEY = 'legion.theme';
const POLL_MS = 15_000;

function readTheme() {
  try {
    const saved = localStorage.getItem(THEME_KEY);
    if (saved === 'light' || saved === 'dark' || saved === 'system') return saved;
  } catch {
    // Ignore.
  }
  return 'system';
}

function applyTheme(theme) {
  const root = document.documentElement;
  if (theme === 'light' || theme === 'dark') {
    root.dataset.theme = theme;
  } else {
    delete root.dataset.theme;
  }
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    // Ignore.
  }
}

function createBrowserCache() {
  try {
    return createCache(window.sessionStorage);
  } catch {
    return createCache(null);
  }
}

function isPairingError(err) {
  const reason = err?.details?.reason;
  return err?.code === 'denied' && (reason === 'auth_failed' || reason === 'host_key_mismatch');
}

async function fetchFleetSnapshot(link, localNode) {
  let fatalError = null;
  const capture = (fallback) => (err) => {
    if (isPairingError(err) && !fatalError) fatalError = err;
    return fallback;
  };

  const [fleetList, fleetPing, localVersion] = await Promise.all([
    link.call('fleet.list', {}).catch(capture({ node: localNode, machines: [] })),
    link.call('fleet.ping', {}).catch(capture({ results: [] })),
    link.call('meta.version', {}).catch(capture(null)),
  ]);
  if (fatalError) throw fatalError;

  const here = fleetList?.node || localNode;
  const listedMap = new Map();
  for (const m of fleetList?.machines || []) {
    listedMap.set(m.name, m);
  }
  if (!listedMap.has(here)) {
    listedMap.set(here, { name: here, this_node: true, systems: [], wake_via: [] });
  }

  const pingMap = new Map();
  for (const r of fleetPing?.results || []) {
    pingMap.set(r.machine, r);
  }

  const allNames = Array.from(listedMap.keys());
  const hasRemote = allNames.some((n) => n !== here);
  const fanTarget = hasRemote ? 'all' : undefined;

  const [sysRaw, jobsRaw, powerRaw] = await Promise.all([
    link.call('sys.info', {}, { on: fanTarget }).catch(() => null),
    link.call('job.list', { state: 'running', limit: 50 }, { on: fanTarget }).catch(() => null),
    link.call('power.status', {}, { on: fanTarget }).catch(() => null),
  ]);

  function byMachine(raw) {
    const map = new Map();
    if (!raw) return map;
    if (isFanOut(raw)) {
      for (const entry of fanOutEntries(raw)) {
        map.set(entry.machine, entry);
      }
    } else {
      map.set(here, { machine: here, ok: true, value: raw, error: null });
    }
    return map;
  }

  const sysMap = byMachine(sysRaw);
  const jobsMap = byMachine(jobsRaw);
  const powerMap = byMachine(powerRaw);

  // Ensure local node always has its own sys.info/job.list even if fleet.toml omits it from "all".
  if (!sysMap.has(here)) {
    const [localSys, localJobs, localPower] = await Promise.all([
      link.call('sys.info', {}).catch(() => null),
      link.call('job.list', { state: 'running', limit: 50 }).catch(() => null),
      link.call('power.status', {}).catch(() => null),
    ]);
    if (localSys) sysMap.set(here, { machine: here, ok: true, value: localSys, error: null });
    if (localJobs) jobsMap.set(here, { machine: here, ok: true, value: localJobs, error: null });
    if (localPower) powerMap.set(here, { machine: here, ok: true, value: localPower, error: null });
  }

  const runningJobs = [];
  const machines = [];

  for (const [name, listed] of listedMap.entries()) {
    const isLocal = name === here || listed.this_node;
    const ping = pingMap.get(name);
    const sysEntry = sysMap.get(name);
    const jobsEntry = jobsMap.get(name);
    const powerEntry = powerMap.get(name);

    const sys = sysEntry?.ok ? sysEntry.value : null;
    const power = powerEntry?.ok ? powerEntry.value : null;
    const jobs = jobsEntry?.ok && Array.isArray(jobsEntry.value?.jobs) ? jobsEntry.value.jobs : [];

    const reachable = isLocal
      ? Boolean(sys || localVersion)
      : ping
        ? Boolean(ping.reachable)
        : Boolean(sysEntry?.ok);

    let error = null;
    let reason = null;
    if (!reachable) {
      error = ping?.error || sysEntry?.error || {
        code: 'unreachable',
        message: 'Did not answer.',
        hint: null,
        machine: name,
      };
      reason = firstSentence(error.message) || 'Did not answer';
    }

    for (const job of jobs) {
      runningJobs.push({ ...job, machine: name });
    }

    const busyReasons = power?.busy?.state === 'busy' ? power.busy.reasons || [] : [];
    const busy = jobs.length > 0 || busyReasons.length > 0;

    machines.push({
      name,
      thisNode: isLocal,
      reachable,
      reason,
      error,
      busy,
      busyReasons,
      runningJobs: jobs.length,
      jobs,
      os: sys?.os?.family || ping?.os || (isLocal ? localVersion?.os : null),
      arch: sys?.os?.arch || ping?.arch || (isLocal ? localVersion?.arch : null),
      version: ping?.version || (isLocal ? localVersion?.version : null),
      system: power?.system || ping?.system || null,
      latencyMs: ping?.latency_ms ?? null,
      canWake: Boolean(listed.mac || (listed.wake_via && listed.wake_via.length > 0)),
      sys,
    });
  }

  // Put this node first, then reachable machines alphabetically, then unreachable ones.
  machines.sort((a, b) => {
    if (a.thisNode !== b.thisNode) return a.thisNode ? -1 : 1;
    if (a.reachable !== b.reachable) return a.reachable ? -1 : 1;
    return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
  });

  runningJobs.sort((a, b) => String(b.created || '').localeCompare(String(a.created || '')));

  return { machines, runningJobs };
}

function ConfirmSheet({ request, onClose }) {
  const [dryResult, setDryResult] = useState(null);
  const [dryError, setDryError] = useState(null);
  const [busy, setBusy] = useState(false);

  async function runDry() {
    if (!request.onDryRun) return;
    setBusy(true);
    setDryError(null);
    try {
      const res = await request.onDryRun();
      setDryResult(res);
    } catch (err) {
      setDryError(err);
    } finally {
      setBusy(false);
    }
  }

  async function runReal() {
    setBusy(true);
    try {
      await request.onConfirm();
      onClose();
    } finally {
      setBusy(false);
    }
  }

  const commands = Array.isArray(dryResult?.commands)
    ? dryResult.commands.map((argv) => (Array.isArray(argv) ? argv.join(' ') : String(argv)))
    : Array.isArray(dryResult?.would_send)
      ? dryResult.would_send.map((addr) => `WoL packet -> ${addr}`)
      : [];

  return html`
    <${Sheet} title=${request.title} onClose=${onClose}>
      <p>${request.summary}</p>
      ${request.jobs?.length > 0
        ? html`
            <div class="list">
              ${request.jobs.map(
                (j) => html`
                  <div class="row" key=${`${j.machine}:${j.id}`}>
                    <span class="body">
                      <span class="title mono">${j.command || j.name || j.id}</span>
                      <span class="sub">
                        running${j.created ? ` · started ${formatRelativeTime(j.created)}` : ''}
                      </span>
                    </span>
                  </div>
                `,
              )}
            </div>
          `
        : null}

      ${dryError ? html`<${Notice} error=${dryError} />` : null}

      ${dryResult
        ? html`
            <div class="spaced">
              ${dryResult.would_refuse
                ? html`<${Notice} error=${dryResult.would_refuse} />`
                : null}
              ${commands.length > 0
                ? html`
                    <p class="label">Dry run commands</p>
                    <pre class="out wrap">${commands.join('\n')}</pre>
                  `
                : html`<pre class="out wrap">${JSON.stringify(dryResult, null, 2)}</pre>`}
            </div>
          `
        : null}

      <div class="actions">
        <button type="button" class="btn" disabled=${busy} onClick=${onClose}>Cancel</button>
        ${request.canDryRun && !dryResult
          ? html`
              <button type="button" class="btn" disabled=${busy} onClick=${runDry}>
                ${busy ? 'Checking…' : 'Try it (dry run)'}
              </button>
            `
          : null}
        <button
          type="button"
          class=${request.danger === false ? 'btn primary full' : 'btn danger full'}
          disabled=${busy}
          onClick=${runReal}
        >
          ${busy ? 'Running…' : request.confirmLabel || 'Confirm'}
        </button>
      </div>
    <//>
  `;
}

function App() {
  const link = useMemo(() => createLink(), []);
  const cache = useMemo(() => createBrowserCache(), []);
  const shellRef = useRef(null);
  const mainRef = useRef(null);

  const [theme, setTheme] = useState(() => readTheme());
  const [wide, setWide] = useState(() => window.innerWidth >= 820);
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  const [booting, setBooting] = useState(true);
  const [linkState, setLinkState] = useState(null);
  const [tools, setTools] = useState(() => cache.get('tools')?.value || []);
  const [snapshot, setSnapshot] = useState(() => cache.get('fleet')?.value || null);
  const [fleetError, setFleetError] = useState(null);
  const [refreshing, setRefreshing] = useState(false);
  const [confirmRequest, setConfirmRequest] = useState(null);
  const [runHistory, setRunHistory] = useState([]);
  const [pairingOpen, setPairingOpen] = useState(false);
  const sheetHistoryRef = useRef(false);

  const nodeInfo = linkState?.nodes?.[0] || null;
  const nodeInfoRef = useRef(nodeInfo);
  nodeInfoRef.current = nodeInfo;
  const wideRef = useRef(wide);
  wideRef.current = wide;
  const allowedSet = useMemo(
    () => (nodeInfo?.whoami?.tools ? new Set(nodeInfo.whoami.tools) : null),
    [nodeInfo],
  );

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  useEffect(() => {
    if (mainRef.current) mainRef.current.scrollTop = 0;
  }, [route.section]);

  useEffect(() => {
    if (!wide && mainRef.current) mainRef.current.scrollTop = 0;
  }, [wide, route.machine, route.tab, route.tool, route.jobId]);

  useEffect(() => {
    const el = shellRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        setWide(entry.contentRect.width >= 820);
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    function syncRoute() {
      const next = parseRoute(window.location.hash);
      setRoute((prev) => {
        if (mainRef.current && (prev.section !== next.section || !wideRef.current)) {
          mainRef.current.scrollTop = 0;
        }
        return next;
      });
    }
    function onPopState() {
      if (sheetHistoryRef.current) {
        sheetHistoryRef.current = false;
        setConfirmRequest(null);
      }
      syncRoute();
    }
    window.addEventListener('hashchange', syncRoute);
    window.addEventListener('popstate', onPopState);
    return () => {
      window.removeEventListener('hashchange', syncRoute);
      window.removeEventListener('popstate', onPopState);
    };
  }, []);

  useEffect(() => {
    const onOpenPairing = () => {
      if (link.kind === 'app') setPairingOpen(true);
    };
    window.addEventListener('legion:open-pairing', onOpenPairing);
    return () => window.removeEventListener('legion:open-pairing', onOpenPairing);
  }, [link]);

  const refreshFleet = useCallback(
    async (activeNodeInfo) => {
      const targetNode = activeNodeInfo || nodeInfoRef.current;
      if (!targetNode) return;
      setRefreshing(true);
      setFleetError(null);
      try {
        const next = await fetchFleetSnapshot(link, targetNode.node);
        setSnapshot(next);
        cache.set('fleet', next);
      } catch (err) {
        setFleetError(err);
      } finally {
        setRefreshing(false);
      }
    },
    [link, cache],
  );

  const hydrate = useCallback(
    async (nextState) => {
      setLinkState(nextState);
      if (!nextState?.ready) {
        setBooting(false);
        return;
      }
      if (nextState.error) {
        setFleetError(nextState.error);
        setBooting(false);
        return;
      }
      const primary = nextState.nodes[0];
      const [toolList] = await Promise.all([
        link.tools({}).catch(() => null),
        refreshFleet(primary),
      ]);
      if (Array.isArray(toolList)) {
        setTools(toolList);
        cache.set('tools', toolList);
      }
      setBooting(false);
    },
    [link, cache, refreshFleet],
  );

  useEffect(() => {
    let alive = true;
    (async () => {
      const { token } = extractHashToken(window.location.hash);
      if (token && typeof link.setToken === 'function') {
        window.history.replaceState(null, '', window.location.pathname + window.location.search + '#/machines');
        setRoute(parseRoute('#/machines'));
        try {
          const next = await link.setToken(token);
          if (alive) await hydrate(next);
          return;
        } catch (err) {
          const fallback = await link.state();
          if (alive) {
            setLinkState({ ...fallback, error: err });
            setBooting(false);
          }
          return;
        }
      }
      const st = await link.state();
      if (alive) await hydrate(st);
    })();
    return () => {
      alive = false;
    };
  }, [link, hydrate]);

  // Poll every 15 s while visible and refresh on window focus.
  useEffect(() => {
    if (!linkState?.ready || !nodeInfo) return;
    const onFocus = () => {
      if (document.visibilityState === 'visible') refreshFleet(nodeInfo);
    };
    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') refreshFleet(nodeInfo);
    }, POLL_MS);
    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onFocus);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onFocus);
    };
  }, [linkState?.ready, nodeInfo, refreshFleet]);

  const openConfirm = useCallback((req) => {
    if (!sheetHistoryRef.current) {
      window.history.pushState({ legionSheet: true }, '');
      sheetHistoryRef.current = true;
    }
    setConfirmRequest(req);
  }, []);

  const closeConfirm = useCallback(() => {
    setConfirmRequest(null);
    if (sheetHistoryRef.current) {
      sheetHistoryRef.current = false;
      window.history.back();
    }
  }, []);

  const recordRun = useCallback((machine, entry) => {
    setRunHistory((prev) => [{ machine, ...entry }, ...prev].slice(0, 20));
  }, []);

  const onWakeMachine = useCallback(
    (machine) => {
      const copy = describePowerConfirm('power.wake', machine.name, { machine: machine.name });
      openConfirm({
        title: copy.title,
        summary: copy.summary,
        tool: 'power.wake',
        effect: 'power',
        confirmLabel: copy.confirmLabel,
        canDryRun: true,
        async onDryRun() {
          return link.call('power.wake', { machine: machine.name, dry_run: true });
        },
        async onConfirm() {
          await link.call('power.wake', { machine: machine.name });
          refreshFleet(nodeInfo);
        },
      });
    },
    [link, openConfirm, refreshFleet, nodeInfo],
  );

  function goBack() {
    if (window.history.length > 1) {
      window.history.back();
    } else if (route.section === 'machines') {
      window.location.hash = '#/machines';
    } else if (route.section === 'jobs') {
      window.location.hash = '#/jobs';
    } else if (route.section === 'tools') {
      window.location.hash = '#/tools';
    }
  }

  function renderHeader() {
    // Narrow sub-pages get a Back button and item title in the top bar.
    if (!wide && route.section === 'machines' && route.machine) {
      const m = (snapshot?.machines || []).find((item) => item.name === route.machine);
      const dotClass = !m?.reachable ? 'dot off' : m.busy ? 'dot busy' : 'dot';
      return html`
        <header class="bar">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <span class=${dotClass} aria-hidden="true"></span>
          <h1>${route.machine}</h1>
          <button
            type="button"
            class="btn icon"
            aria-label="Refresh"
            onClick=${() => refreshFleet(nodeInfo)}
          >
            <${Icon} name="refresh" />
          </button>
        </header>
      `;
    }

    if (!wide && route.section === 'tools' && route.tool) {
      const spec = (tools || []).find((t) => t.name === route.tool);
      return html`
        <header class="bar">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <h1>${route.tool}</h1>
          ${spec ? html`<span class=${`chip fx ${spec.effect}`}>${spec.effect}</span>` : null}
        </header>
      `;
    }

    if (!wide && route.section === 'jobs' && route.jobId) {
      return html`
        <header class="bar">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <h1>${route.jobId}</h1>
        </header>
      `;
    }

    return html`
      <header class="bar">
        <h1 class="wordmark"><b>▍</b>Legion</h1>
        ${nodeInfo?.role ? html`<span class="chip">${nodeInfo.role}</span>` : null}
        <button
          type="button"
          class="btn icon"
          aria-label="Refresh"
          onClick=${() => refreshFleet(nodeInfo)}
        >
          <${Icon} name="refresh" />
        </button>
      </header>
    `;
  }

  return html`
    <div ref=${shellRef} class="shell">
      <${IconSprite} />
      ${booting
        ? html`
            <div class="app">
              <header class="bar"><h1 class="wordmark"><b>▍</b>Legion</h1></header>
              <main class="main"><div class="empty">Connecting…</div></main>
            </div>
          `
        : !linkState?.ready || pairingOpen
          ? html`
              <${StartView}
                link=${link}
                linkState=${linkState}
                onCancel=${linkState?.ready ? () => setPairingOpen(false) : null}
                onConnected=${(next) => {
                  setPairingOpen(false);
                  setFleetError(null);
                  hydrate(next);
                }}
              />
            `
          : html`
              <div class="app">
                ${renderHeader()}
                <main ref=${mainRef} class="main">
                  ${route.section === 'machines'
                    ? html`
                        <${MachinesView}
                          snapshot=${snapshot}
                          error=${fleetError}
                          loading=${refreshing}
                          wide=${wide}
                          route=${route}
                          link=${link}
                          nodeInfo=${nodeInfo}
                          tools=${tools}
                          allowedSet=${allowedSet}
                          onWakeMachine=${onWakeMachine}
                          onRequestConfirm=${openConfirm}
                          onRefresh=${() => refreshFleet(nodeInfo)}
                          runHistory=${runHistory}
                          onRecordRun=${recordRun}
                        />
                      `
                    : route.section === 'jobs'
                      ? html`
                          <${JobsView}
                            route=${route}
                            snapshot=${snapshot}
                            link=${link}
                            nodeInfo=${nodeInfo}
                            allowedSet=${allowedSet}
                            wide=${wide}
                            onRefreshFleet=${() => refreshFleet(nodeInfo)}
                          />
                        `
                      : route.section === 'tools'
                        ? html`
                            <${ToolsView}
                              route=${route}
                              tools=${tools}
                              allowedSet=${allowedSet}
                              nodeInfo=${nodeInfo}
                              snapshot=${snapshot}
                              link=${link}
                              wide=${wide}
                              onRequestConfirm=${openConfirm}
                            />
                          `
                        : html`
                            <${DeviceView}
                              link=${link}
                              linkState=${linkState}
                              nodeInfo=${nodeInfo}
                              tools=${tools}
                              theme=${theme}
                              onChangeTheme=${setTheme}
                              onRequestConfirm=${openConfirm}
                              onOpenPairing=${link.kind === 'app' ? () => setPairingOpen(true) : null}
                              onSelectNode=${
                                link.kind === 'app' && typeof link.selectNode === 'function'
                                  ? async (node) => {
                                      cache.clear();
                                      setSnapshot(null);
                                      setTools([]);
                                      const next = await link.selectNode(node);
                                      await hydrate(next);
                                    }
                                  : null
                              }
                              onDisconnected=${async () => {
                                cache.clear();
                                setSnapshot(null);
                                setTools([]);
                                const st = await link.state();
                                await hydrate(st);
                              }}
                            />
                          `}
                </main>
                <nav class="nav" aria-label="Main">
                  <a
                    href="#/machines"
                    aria-current=${route.section === 'machines' ? 'page' : undefined}
                  >
                    <${Icon} name="machines" />Machines
                  </a>
                  <a
                    href="#/jobs"
                    aria-current=${route.section === 'jobs' ? 'page' : undefined}
                  >
                    <${Icon} name="jobs" />Jobs
                  </a>
                  <a
                    href="#/tools"
                    aria-current=${route.section === 'tools' ? 'page' : undefined}
                  >
                    <${Icon} name="tools" />Tools
                  </a>
                  <a
                    href="#/device"
                    aria-current=${route.section === 'device' ? 'page' : undefined}
                  >
                    <${Icon} name="device" />${wide ? 'Device' : 'This device'}
                  </a>
                </nav>
              </div>
            `}
      ${confirmRequest
        ? html`<${ConfirmSheet} request=${confirmRequest} onClose=${closeConfirm} />`
        : null}
    </div>
  `;
}

render(html`<${App} />`, document.getElementById('root'));
