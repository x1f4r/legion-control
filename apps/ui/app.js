// Legion interface entry point: startup, hash routing, polling, confirmation sheet, and shell.

import { html, render, useCallback, useEffect, useMemo, useRef, useState } from './vendor/preact-htm.js';
import { Icon, IconSprite } from './icons.js';
import { createCache } from './lib/cache.js';
import {
  hydrateFleetCache,
  refreshFleetProgressive,
  serializeFleetCache,
} from './lib/fleet.js';
import { formatRelativeTime } from './lib/format.js';
import { extractHashToken, parseRoute } from './lib/route.js';
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

function createSessionCache() {
  try {
    return createCache(window.sessionStorage);
  } catch {
    return createCache(null);
  }
}

function createPersistentCache() {
  try {
    return createCache(window.localStorage);
  } catch {
    return createCache(null);
  }
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
  const sessionCache = useMemo(() => createSessionCache(), []);
  const persistentCache = useMemo(() => createPersistentCache(), []);
  const shellRef = useRef(null);
  const mainRef = useRef(null);
  const refreshAbortRef = useRef(null);
  const refreshInFlightRef = useRef(false);

  const [theme, setTheme] = useState(() => readTheme());
  const [wide, setWide] = useState(() => window.innerWidth >= 820);
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  const [booting, setBooting] = useState(true);
  const [linkState, setLinkState] = useState(null);
  const [tools, setTools] = useState(() => sessionCache.get('tools')?.value || []);
  const [snapshot, setSnapshot] = useState(null);
  const [fleetError, setFleetError] = useState(null);
  const [refreshing, setRefreshing] = useState(false);
  const [confirmRequest, setConfirmRequest] = useState(null);
  const [runHistory, setRunHistory] = useState([]);
  const [pairingOpen, setPairingOpen] = useState(false);
  const sheetHistoryRef = useRef(false);

  const nodeInfo = linkState?.nodes?.[0] || null;
  const nodeInfoRef = useRef(nodeInfo);
  nodeInfoRef.current = nodeInfo;
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;
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
    async (activeNodeInfo, options = {}) => {
      const targetNode = activeNodeInfo || nodeInfoRef.current;
      if (!targetNode) return;
      if (options.background && refreshInFlightRef.current) return;

      if (refreshAbortRef.current) {
        refreshAbortRef.current.abort();
      }
      const ctrl = new AbortController();
      refreshAbortRef.current = ctrl;
      refreshInFlightRef.current = true;
      setRefreshing(true);
      setFleetError(null);

      const baseSnap = options.initialSnapshot !== undefined ? options.initialSnapshot : snapshotRef.current;

      try {
        await refreshFleetProgressive({
          link,
          localNode: targetNode.node,
          initialMachines: baseSnap?.machines || [],
          cachedAt: baseSnap?.cachedAt ?? null,
          signal: ctrl.signal,
          onUpdate(next) {
            if (ctrl.signal.aborted) return;
            setSnapshot(next);
          },
          onSettled(finalSnap) {
            if (ctrl.signal.aborted) return;
            if (link.kind !== 'mock') {
              persistentCache.set('fleet', serializeFleetCache(targetNode.node, finalSnap.machines));
            }
          },
        });
      } catch (err) {
        if (!ctrl.signal.aborted) setFleetError(err);
      } finally {
        if (refreshAbortRef.current === ctrl) {
          refreshAbortRef.current = null;
          refreshInFlightRef.current = false;
          setRefreshing(false);
        }
      }
    },
    [link, persistentCache],
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
      let cachedSnap = null;
      if (link.kind !== 'mock') {
        const stored = persistentCache.get('fleet')?.value;
        cachedSnap = hydrateFleetCache(stored, primary.node);
        if (cachedSnap) {
          setSnapshot(cachedSnap);
        }
      }
      // Paint immediately with cached list or this node; never block first paint on remote peers.
      setBooting(false);

      link
        .tools({})
        .then((toolList) => {
          if (Array.isArray(toolList)) {
            setTools(toolList);
            sessionCache.set('tools', toolList);
          }
        })
        .catch(() => {});

      refreshFleet(primary, { initialSnapshot: cachedSnap });
    },
    [link, sessionCache, persistentCache, refreshFleet],
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
      if (refreshAbortRef.current) {
        refreshAbortRef.current.abort();
        refreshAbortRef.current = null;
      }
    };
  }, [link, hydrate]);

  // Poll every 15 s while visible and refresh on resume/focus. On mobile resume from background,
  // tear down cached SSH sessions before refreshing so waking a phone does not stall on half-open
  // TCP sockets.
  useEffect(() => {
    if (!linkState?.ready || !nodeInfo) return;
    let wasHidden = document.visibilityState === 'hidden';
    let resuming = false;

    const onVisibilityChange = async () => {
      if (document.visibilityState === 'hidden') {
        wasHidden = true;
        return;
      }
      if (document.visibilityState !== 'visible') return;
      if (wasHidden && typeof link.invalidateSessions === 'function') {
        wasHidden = false;
        resuming = true;
        try {
          await link.invalidateSessions();
        } finally {
          resuming = false;
        }
        refreshFleet(nodeInfo, { background: true });
      } else {
        wasHidden = false;
        refreshFleet(nodeInfo, { background: true });
      }
    };

    const onFocus = () => {
      if (document.visibilityState === 'visible' && !wasHidden && !resuming) {
        refreshFleet(nodeInfo, { background: true });
      }
    };

    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible' && !resuming) {
        refreshFleet(nodeInfo, { background: true });
      }
    }, POLL_MS);
    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onVisibilityChange);
    };
  }, [link, linkState?.ready, nodeInfo, refreshFleet]);

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
      const dotClass =
        m?.status === 'asking' || m?.asking
          ? 'dot asking'
          : !m?.reachable
            ? 'dot off'
            : m.busy
              ? 'dot busy'
              : 'dot';
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
                                if (refreshAbortRef.current) {
                                  refreshAbortRef.current.abort();
                                  refreshAbortRef.current = null;
                                }
                                refreshInFlightRef.current = false;
                                sessionCache.clear();
                                persistentCache.clear();
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
