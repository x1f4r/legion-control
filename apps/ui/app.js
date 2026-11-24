// Legion interface entry point: startup, hash routing, settings, gestures, polling, and shell.

import { html, render, useCallback, useEffect, useMemo, useRef, useState } from './vendor/preact-htm.js';
import { Icon, IconSprite } from './icons.js';
import { findActionById } from './lib/actions.js';
import {
  describeDryRunSummary,
  formatMachineDetailSubtitle,
  formatMachinesHeaderSubtitle,
  friendlyJobName,
  plainRefusalParts,
} from './lib/activity.js';
import { createCache } from './lib/cache.js';
import {
  hydrateFleetCache,
  refreshFleetProgressive,
  serializeFleetCache,
} from './lib/fleet.js';
import { formatRelativeTime } from './lib/format.js';
import {
  classifyHorizontalSwipe,
  classifyPullRefresh,
  hasNativeEdgeBack,
  PULL_REFRESH_PX,
  shouldIgnoreSwipeTarget,
  stepTab,
} from './lib/gestures.js';
import {
  extractHashToken,
  MACHINE_PARTS,
  parseRoute,
  ROOT_SECTIONS,
  routeHref,
} from './lib/route.js';
import { describePowerConfirm } from './lib/schema.js';
import {
  applySettingsToRoot,
  detectDeviceNoun,
  readSettings,
  writeSettings,
} from './lib/settings.js';
import { createLink } from './link/index.js';
import { ActionsView } from './views/actions.js';
import { ActivityView } from './views/activity.js';
import { Notice, Sheet } from './views/common.js';
import { CommandComposer } from './views/machine.js';
import { MachinesView } from './views/machines.js';
import { SettingsView } from './views/settings.js';
import { StartView } from './views/start.js';

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

function safeLocalStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const WAKE_TTL_MS = 120_000;

function ConfirmSheet({ request, technical, onClose }) {
  const [dryResult, setDryResult] = useState(null);
  const [dryError, setDryError] = useState(null);
  const [realError, setRealError] = useState(null);
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
    setRealError(null);
    try {
      await request.onConfirm();
      onClose();
    } catch (err) {
      setRealError(err);
    } finally {
      setBusy(false);
    }
  }

  const commands = Array.isArray(dryResult?.commands)
    ? dryResult.commands.map((argv) => (Array.isArray(argv) ? argv.join(' ') : String(argv)))
    : Array.isArray(dryResult?.would_send)
      ? dryResult.would_send.map((addr) => `WoL packet -> ${addr}`)
      : [];
  const refusalParts = dryResult?.would_refuse ? plainRefusalParts(dryResult.would_refuse) : null;
  const plainDrySummary = dryResult
    ? describeDryRunSummary(request.tool, request.targetName || request.target, request.args)
    : '';

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
                      <span class=${technical ? 'title mono' : 'title'}>
                        ${technical ? j.command || j.name || j.id : friendlyJobName(j)}
                      </span>
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
      ${realError ? html`<${Notice} error=${realError} />` : null}

      ${dryResult
        ? html`
            <div class="spaced">
              ${dryResult.would_refuse
                ? technical
                  ? html`
                      <div class="notice neutral">
                        <strong>It would refuse right now</strong>
                        <span>${dryResult.would_refuse.message || String(dryResult.would_refuse)}</span>
                        ${dryResult.would_refuse.hint
                          ? html`<span class="hint">${dryResult.would_refuse.hint}</span>`
                          : null}
                      </div>
                    `
                  : html`
                      <div class="notice neutral">
                        <strong>It would refuse right now: ${refusalParts.first}</strong>
                        ${refusalParts.detailMessage || refusalParts.detailHint
                          ? html`
                              <details class="more">
                                <summary>More</summary>
                                ${refusalParts.detailMessage
                                  ? html`<p>${refusalParts.detailMessage}</p>`
                                  : null}
                                ${refusalParts.detailHint
                                  ? html`<p>${refusalParts.detailHint}</p>`
                                  : null}
                              </details>
                            `
                          : null}
                      </div>
                    `
                : !technical
                  ? html`
                      <div class="notice neutral">
                        <strong>${plainDrySummary}</strong>
                      </div>
                    `
                  : null}
              ${technical
                ? commands.length > 0
                  ? html`
                      <p class="label">Dry run commands</p>
                      <pre class="out wrap">${commands.join('\n')}</pre>
                    `
                  : !dryResult.would_refuse
                    ? html`<pre class="out wrap">${JSON.stringify(dryResult, null, 2)}</pre>`
                    : null
                : null}
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
  const swipeRef = useRef(null);

  const [settings, setSettings] = useState(() => {
    const loaded = readSettings(safeLocalStorage());
    if (link.mockFlags?.has('tech')) {
      return { ...loaded, technical: true };
    }
    return loaded;
  });
  const [viewportWidth, setViewportWidth] = useState(() => window.innerWidth);
  const wide = viewportWidth >= 820;
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  const [booting, setBooting] = useState(true);
  const [linkState, setLinkState] = useState(null);
  const [tools, setTools] = useState(() => sessionCache.get('tools')?.value || []);
  const [snapshot, setSnapshot] = useState(null);
  const [fleetError, setFleetError] = useState(null);
  const [refreshing, setRefreshing] = useState(false);
  const [pullState, setPullState] = useState(''); // '' | 'pull' | 'release' | 'refreshing'
  const [confirmRequest, setConfirmRequest] = useState(null);
  const [actionsQuery, setActionsQuery] = useState('');
  const [commandRunsByMachine, setCommandRunsByMachine] = useState({});
  const [commandDraft, setCommandDraft] = useState({
    cmd: '',
    asJob: false,
    jobName: '',
    running: false,
    error: null,
  });
  const [pairingOpen, setPairingOpen] = useState(false);
  const [pairNotice, setPairNotice] = useState(null);
  const [wakeSentAt, setWakeSentAt] = useState({});
  const sheetHistoryRef = useRef(false);
  const learnInFlightRef = useRef(false);

  const nodeInfo = linkState?.nodes?.[0] || null;
  const nodeInfoRef = useRef(nodeInfo);
  nodeInfoRef.current = nodeInfo;
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;
  const wideRef = useRef(wide);
  wideRef.current = wide;
  const settingsRef = useRef(settings);
  settingsRef.current = settings;

  const allowedSet = useMemo(
    () => (nodeInfo?.whoami?.tools ? new Set(nodeInfo.whoami.tools) : null),
    [nodeInfo],
  );

  const deviceNoun = useMemo(
    () => detectDeviceNoun(link.kind, typeof navigator !== 'undefined' ? navigator.userAgent : '', viewportWidth),
    [link.kind, viewportWidth],
  );

  const updateSettings = useCallback((patch) => {
    setSettings((prev) => {
      const next = writeSettings(safeLocalStorage(), { ...prev, ...patch });
      applySettingsToRoot(document.documentElement, next);
      return next;
    });
  }, []);

  useEffect(() => {
    applySettingsToRoot(document.documentElement, settings);
  }, [settings]);

  useEffect(() => {
    if (mainRef.current) mainRef.current.scrollTop = 0;
  }, [route.section]);

  useEffect(() => {
    if (!wide && mainRef.current && route.tab !== 'command') {
      mainRef.current.scrollTop = 0;
    }
  }, [wide, route.machine, route.tab, route.tool, route.action, route.jobId, route.page, route.sub]);

  useEffect(() => {
    const el = shellRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        setViewportWidth(entry.contentRect.width);
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

  const mergeBackendState = useCallback((updated) => {
    if (!updated || !Array.isArray(updated.nodes)) return;
    setLinkState((prev) => {
      if (!prev) return updated;
      const prevByNode = new Map((prev.nodes || []).map((n) => [n.node, n]));
      const mergedNodes = updated.nodes.map((n) => {
        const old = prevByNode.get(n.node);
        if (!old) return n;
        return {
          ...n,
          version: n.version || old.version,
          build: n.build ?? old.build,
          protocol: n.protocol ?? old.protocol,
          whoami: n.whoami || old.whoami,
        };
      });
      return {
        ...prev,
        ...updated,
        nodes: mergedNodes,
      };
    });
  }, []);

  const triggerLearnInBackground = useCallback(() => {
    if (typeof link.learn !== 'function' || learnInFlightRef.current) return;
    learnInFlightRef.current = true;
    link
      .learn()
      .then((res) => {
        if (res?.state) mergeBackendState(res.state);
      })
      .catch(() => {})
      .finally(() => {
        learnInFlightRef.current = false;
      });
  }, [link, mergeBackendState]);

  const handleActiveNodeChanged = useCallback(
    (nextNode) => {
      if (!nextNode) return;
      setLinkState((prev) => {
        if (!prev || !Array.isArray(prev.nodes)) return prev;
        const idx = prev.nodes.findIndex((n) => n.node === nextNode);
        if (idx <= 0) return prev;
        const reordered = [prev.nodes[idx], ...prev.nodes.slice(0, idx), ...prev.nodes.slice(idx + 1)];
        return { ...prev, nodes: reordered };
      });
    },
    [],
  );

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
          forceOffline: Boolean(options.forceOffline),
          checkOffline: settingsRef.current.checkOffline !== false,
          signal: ctrl.signal,
          onActiveNodeChanged: handleActiveNodeChanged,
          onUpdate(next) {
            if (ctrl.signal.aborted) return;
            setSnapshot(next);
          },
          onSettled(finalSnap) {
            if (ctrl.signal.aborted) return;
            if (link.kind !== 'mock') {
              const activeName = nodeInfoRef.current?.node || targetNode.node;
              persistentCache.set('fleet', serializeFleetCache(activeName, finalSnap.machines));
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
          setPullState('');
        }
      }
    },
    [link, persistentCache, handleActiveNodeChanged],
  );

  const handleAcceptIdentityChange = useCallback(
    async (nodeName) => {
      if (typeof link.acceptIdentityChange !== 'function') return;
      const next = await link.acceptIdentityChange(nodeName);
      if (next) mergeBackendState(next);
      refreshFleet(nodeInfoRef.current, { background: true });
    },
    [link, mergeBackendState, refreshFleet],
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
      if (typeof link.initialFleetCache === 'function') {
        cachedSnap = hydrateFleetCache(link.initialFleetCache(), primary.node);
        if (cachedSnap) setSnapshot(cachedSnap);
      } else if (link.kind !== 'mock') {
        const stored = persistentCache.get('fleet')?.value;
        cachedSnap = hydrateFleetCache(stored, primary.node);
        if (cachedSnap) setSnapshot(cachedSnap);
      }
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
      triggerLearnInBackground();
    },
    [link, sessionCache, persistentCache, refreshFleet, triggerLearnInBackground],
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

  // Poll on the user's configured interval while visible and refresh on resume/focus.
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
      if (document.visibilityState === 'visible' && !wasHidden && !resuming && settings.refreshSeconds > 0) {
        refreshFleet(nodeInfo, { background: true });
      }
    };

    const timer =
      settings.refreshSeconds > 0
        ? window.setInterval(() => {
            if (document.visibilityState === 'visible' && !resuming) {
              refreshFleet(nodeInfo, { background: true });
            }
          }, settings.refreshSeconds * 1000)
        : null;

    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => {
      if (timer !== null) window.clearInterval(timer);
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onVisibilityChange);
    };
  }, [link, linkState?.ready, nodeInfo, refreshFleet, settings.refreshSeconds]);

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

  const commandRunsApi = useMemo(
    () => ({
      setDraft(patch) {
        setCommandDraft((prev) => ({ ...prev, ...patch }));
      },
      appendRun(machineName, entry) {
        setCommandRunsByMachine((prev) => ({
          ...prev,
          [machineName]: [...(prev[machineName] || []), entry].slice(-25),
        }));
      },
      updateRun(machineName, runId, updater) {
        setCommandRunsByMachine((prev) => {
          const list = prev[machineName] || [];
          return {
            ...prev,
            [machineName]: list.map((item) => (item.id === runId ? updater(item) : item)),
          };
        });
      },
    }),
    [],
  );

  const markWakeSent = useCallback((name) => {
    if (!name) return;
    setWakeSentAt((prev) => ({ ...prev, [name]: Date.now() }));
  }, []);

  useEffect(() => {
    const entries = Object.entries(wakeSentAt);
    if (entries.length === 0) return;
    const now = Date.now();
    const reachableNames = new Set(
      (snapshot?.machines || []).filter((m) => m.reachable).map((m) => m.name),
    );
    let changed = false;
    const next = {};
    let soonestMs = Infinity;
    for (const [name, ts] of entries) {
      const remaining = WAKE_TTL_MS - (now - ts);
      if (reachableNames.has(name) || remaining <= 0) {
        changed = true;
      } else {
        next[name] = ts;
        if (remaining < soonestMs) soonestMs = remaining;
      }
    }
    if (changed) {
      setWakeSentAt(next);
      return;
    }
    const timer = window.setTimeout(() => {
      const t = Date.now();
      setWakeSentAt((prev) => {
        let dirty = false;
        const filtered = {};
        for (const [name, ts] of Object.entries(prev)) {
          if (t - ts >= WAKE_TTL_MS) dirty = true;
          else filtered[name] = ts;
        }
        return dirty ? filtered : prev;
      });
    }, Math.max(250, soonestMs + 50));
    return () => window.clearTimeout(timer);
  }, [wakeSentAt, snapshot]);

  const wakePendingSet = useMemo(() => {
    const now = Date.now();
    const out = new Set();
    for (const [name, ts] of Object.entries(wakeSentAt)) {
      if (now - ts < WAKE_TTL_MS) out.add(name);
    }
    return out;
  }, [wakeSentAt]);

  const onWakeMachine = useCallback(
    (machine) => {
      const args = { machine: machine.name };
      const copy = describePowerConfirm('power.wake', machine.name, args);
      openConfirm({
        title: copy.title,
        summary: copy.summary,
        targetName: machine.name,
        args,
        tool: 'power.wake',
        effect: 'power',
        confirmLabel: copy.confirmLabel,
        canDryRun: true,
        async onDryRun() {
          return link.call('power.wake', { machine: machine.name, dry_run: true });
        },
        async onConfirm() {
          await link.call('power.wake', { machine: machine.name });
          markWakeSent(machine.name);
          refreshFleet(nodeInfo, { forceOffline: true });
        },
      });
    },
    [link, openConfirm, markWakeSent, refreshFleet, nodeInfo],
  );

  const machines = snapshot?.machines || [];
  const selectedMachine = route.machine
    ? machines.find((m) => m.name === route.machine) || null
    : null;

  const isRootPlace =
    (route.section === 'machines' && !route.machine) ||
    (route.section === 'activity' && !route.jobId) ||
    (route.section === 'actions' && !route.action && !route.allTools);

  const inMachinePhone = !wide && route.section === 'machines' && Boolean(route.machine);
  const isPhoneCommand =
    inMachinePhone && route.tab === 'command' && Boolean(selectedMachine?.reachable);
  const isActionsRoot =
    route.section === 'actions' && !route.action && !route.allTools;
  const hasDock = isActionsRoot || isPhoneCommand;

  // Hide bottom bar on phone for Settings and single-item detail pages so Settings feels like a real page.
  const showBottomNav = wide || isRootPlace || inMachinePhone;

  function goBack() {
    if (route.section === 'machines' && route.machine) {
      if (route.jobId) {
        window.location.hash = routeHref({ section: 'machines', machine: route.machine, tab: 'activity' });
        return;
      }
      if (['command', 'services', 'projects', 'activity'].includes(route.tab)) {
        window.location.hash = routeHref({ section: 'machines', machine: route.machine, tab: 'more' });
        return;
      }
      window.location.hash = '#/machines';
      return;
    }
    if (route.section === 'activity' && route.jobId) {
      window.location.hash = '#/activity';
      return;
    }
    if (route.section === 'actions') {
      if (route.allTools && route.tool) {
        window.location.hash = '#/actions/all';
        return;
      }
      window.location.hash = '#/actions';
      return;
    }
    if (route.section === 'settings') {
      if (route.sub) {
        window.location.hash = routeHref({ section: 'settings', page: route.page });
        return;
      }
      if (route.page) {
        window.location.hash = '#/settings';
        return;
      }
      window.location.hash = '#/machines';
      return;
    }
    if (window.history.length > 1) {
      window.history.back();
    } else {
      window.location.hash = '#/machines';
    }
  }

  // Pointer-event gestures: root-place swipe, machine-part swipe, pull-to-refresh, non-Android edge back.
  function onMainPointerDown(e) {
    if (e.button > 0) return;
    if (shouldIgnoreSwipeTarget(e.target, mainRef.current)) return;
    swipeRef.current = {
      id: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      t: Date.now(),
      scrollTop: mainRef.current ? mainRef.current.scrollTop : 0,
    };
  }

  function onMainPointerMove(e) {
    const start = swipeRef.current;
    if (!start || start.id !== e.pointerId) return;
    const dx = e.clientX - start.x;
    const dy = e.clientY - start.y;
    const canPull =
      (route.section === 'machines' && (!route.machine || route.tab === 'overview')) ||
      (route.section === 'activity' && !route.jobId);
    if (canPull && start.scrollTop <= 2 && dy > 20 && dy > Math.abs(dx) * 1.5) {
      setPullState(dy >= PULL_REFRESH_PX ? 'release' : 'pull');
    } else if (pullState === 'pull' || pullState === 'release') {
      setPullState('');
    }
  }

  function onMainPointerUp(e) {
    const start = swipeRef.current;
    swipeRef.current = null;
    if (!start || start.id !== e.pointerId) return;

    const dx = e.clientX - start.x;
    const dy = e.clientY - start.y;
    const elapsedMs = Date.now() - start.t;

    const canPull =
      (route.section === 'machines' && (!route.machine || route.tab === 'overview')) ||
      (route.section === 'activity' && !route.jobId);
    if (canPull && classifyPullRefresh({ dx, dy, scrollTop: start.scrollTop })) {
      setPullState('refreshing');
      refreshFleet(nodeInfo, { forceOffline: true });
      triggerLearnInBackground();
      return;
    }
    if (pullState === 'pull' || pullState === 'release') {
      setPullState('');
    }

    if (wide) return;

    const allowEdgeBack = !isRootPlace && !hasNativeEdgeBack();
    const swipe = classifyHorizontalSwipe({
      dx,
      dy,
      elapsedMs,
      startX: start.x,
      allowEdgeBack,
    });
    if (!swipe) return;

    if (swipe === 'edge-back') {
      goBack();
      return;
    }

    if (isRootPlace) {
      const nextSection = stepTab(ROOT_SECTIONS, route.section, swipe);
      if (nextSection) {
        window.location.hash = routeHref({ section: nextSection });
      }
      return;
    }

    if (inMachinePhone && route.machine) {
      const currentPart = MACHINE_PARTS.includes(route.tab) ? route.tab : 'more';
      const nextPart = stepTab(MACHINE_PARTS, currentPart, swipe);
      if (nextPart) {
        window.location.hash = routeHref({
          section: 'machines',
          machine: route.machine,
          tab: nextPart,
        });
      }
    }
  }

  function onMainPointerCancel() {
    swipeRef.current = null;
    if (pullState === 'pull' || pullState === 'release') setPullState('');
  }

  function settingsHeaderTitle() {
    if (!route.page) return 'Settings';
    if (route.page === 'device') return `This ${deviceNoun}`;
    if (route.page === 'connections') {
      if (route.sub === 'pair') return 'Pair a machine';
      if (route.sub === 'invite') return 'Let another device in';
      if (route.sub === 'add') return 'Add a machine';
      if (route.sub === 'node' && route.item) return route.item;
      if (route.sub === 'incoming' && route.item) return route.item;
      return 'Connections';
    }
    if (route.page === 'appearance') return 'Appearance';
    if (route.page === 'refreshing') return 'Refreshing';
    if (route.page === 'about') return 'About';
    return 'Settings';
  }

  function renderHeader() {
    // Phone inside a machine.
    if (inMachinePhone && route.machine) {
      const m = selectedMachine;
      const dotClass =
        m?.status === 'asking' || m?.asking
          ? 'dot asking'
          : !m?.reachable
            ? 'dot off'
            : m.busy
              ? 'dot busy'
              : 'dot';
      const subTabNames = {
        command: 'Command',
        services: 'Services',
        projects: 'Projects',
        activity: 'Activity',
      };
      if (subTabNames[route.tab]) {
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>
              ${subTabNames[route.tab]}
              <small>${route.machine}</small>
            </h1>
          </header>
        `;
      }
      return html`
        <header class="top sub">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <span class=${dotClass} aria-hidden="true"></span>
          <h1>
            ${route.machine}
            ${m
              ? html`<small>
                  ${formatMachineDetailSubtitle(m, Date.now(), {
                    wakePending: wakePendingSet.has(m.name),
                  })}
                </small>`
              : null}
          </h1>
        </header>
      `;
    }

    // Phone Activity detail.
    if (!wide && route.section === 'activity' && route.jobId) {
      return html`
        <header class="top sub">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <h1>Activity</h1>
        </header>
      `;
    }

    // Actions sub-pages (All tools or one Action).
    if (route.section === 'actions' && route.allTools) {
      const spec = route.tool ? (tools || []).find((t) => t.name === route.tool) : null;
      return html`
        <header class="top sub">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <h1>${!wide && route.tool ? route.tool : 'All tools'}</h1>
          ${!wide && spec ? html`<span class=${`chip fx ${spec.effect}`}>${spec.effect}</span>` : null}
        </header>
      `;
    }

    if (route.section === 'actions' && route.action) {
      const act = findActionById(route.action, tools, allowedSet, nodeInfo?.whoami);
      return html`
        <header class="top sub">
          <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
            <${Icon} name="back" />
          </button>
          <h1>${act ? act.title : 'Action'}</h1>
        </header>
      `;
    }

    // Settings (phone gets Back + title; wide gets big "Settings" title, or sub-back for deep connections flows).
    if (route.section === 'settings') {
      if (!wide || route.sub) {
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>${settingsHeaderTitle()}</h1>
          </header>
        `;
      }
      return html`
        <header class="top">
          <h1>Settings</h1>
        </header>
      `;
    }

    // Root places: Machines, Activity, Actions.
    const gearBtn = !wide
      ? html`
          <a class="btn icon" href="#/settings" aria-label="Settings">
            <${Icon} name="gear" />
          </a>
        `
      : null;

    if (route.section === 'activity') {
      const runningCount = (snapshot?.runningJobs || []).length;
      return html`
        <header class="top">
          <h1>
            Activity
            ${runningCount > 0 ? html`<small>${runningCount} running now</small>` : null}
          </h1>
          ${gearBtn}
        </header>
      `;
    }

    if (route.section === 'actions') {
      return html`
        <header class="top">
          <h1>Actions</h1>
          ${gearBtn}
        </header>
      `;
    }

    const machinesSub = formatMachinesHeaderSubtitle(machines, {
      cachedAt: snapshot?.cachedAt ?? null,
    });
    return html`
      <header class="top">
        <h1>
          Machines
          ${machinesSub ? html`<small>${machinesSub}</small>` : null}
        </h1>
        ${gearBtn}
      </header>
    `;
  }

  function renderDock() {
    if (isActionsRoot) {
      return html`
        <div class="dock">
          <input
            class="input plain"
            type="search"
            value=${actionsQuery}
            onInput=${(e) => setActionsQuery(e.target.value)}
            placeholder="Find something to do"
            aria-label="Find something to do"
          />
        </div>
      `;
    }
    if (isPhoneCommand && selectedMachine) {
      return html`
        <div class="dock">
          <${CommandComposer}
            machine=${selectedMachine}
            nodeInfo=${nodeInfo}
            link=${link}
            allowedSet=${allowedSet}
            state=${commandDraft}
            onStateChange=${commandRunsApi.setDraft}
            onAppendRun=${commandRunsApi.appendRun}
            onUpdateRun=${commandRunsApi.updateRun}
            onOpenJob=${(id) => {
              window.location.hash = routeHref({
                section: 'machines',
                machine: selectedMachine.name,
                tab: 'activity',
                jobId: id,
              });
            }}
          />
        </div>
      `;
    }
    return null;
  }

  function renderNav() {
    if (!showBottomNav) return null;

    // Phone inside a machine: 4 parts of this machine.
    if (inMachinePhone && route.machine) {
      const moreActive = ['more', 'command', 'services', 'projects', 'activity'].includes(route.tab);
      return html`
        <nav class="nav parts" aria-label=${`${route.machine} sections`}>
          <a
            href=${routeHref({ section: 'machines', machine: route.machine, tab: 'overview' })}
            aria-current=${route.tab === 'overview' ? 'page' : undefined}
          >
            <${Icon} name="overview" />Overview
          </a>
          <a
            href=${routeHref({ section: 'machines', machine: route.machine, tab: 'files' })}
            aria-current=${route.tab === 'files' ? 'page' : undefined}
          >
            <${Icon} name="files" />Files
          </a>
          <a
            href=${routeHref({ section: 'machines', machine: route.machine, tab: 'power' })}
            aria-current=${route.tab === 'power' ? 'page' : undefined}
          >
            <${Icon} name="power" />Power
          </a>
          <a
            href=${routeHref({ section: 'machines', machine: route.machine, tab: 'more' })}
            aria-current=${moreActive ? 'page' : undefined}
          >
            <${Icon} name="more" />More
          </a>
        </nav>
      `;
    }

    // Root 3 places (plus Settings at the foot of the wide rail).
    return html`
      <nav class="nav" aria-label="Main">
        <a
          href="#/machines"
          aria-current=${route.section === 'machines' ? 'page' : undefined}
        >
          <${Icon} name="machines" />Machines
        </a>
        <a
          href="#/activity"
          aria-current=${route.section === 'activity' ? 'page' : undefined}
        >
          <${Icon} name="activity" />Activity
        </a>
        <a
          href="#/actions"
          aria-current=${route.section === 'actions' ? 'page' : undefined}
        >
          <${Icon} name="actions" />Actions
        </a>
        ${wide
          ? html`
              <a
                class="last"
                href="#/settings"
                aria-current=${route.section === 'settings' ? 'page' : undefined}
              >
                <${Icon} name="gear" />Settings
              </a>
            `
          : null}
      </nav>
    `;
  }

  const appClasses = [
    'app',
    !showBottomNav ? 'standalone' : '',
    hasDock ? 'docked' : '',
  ]
    .filter(Boolean)
    .join(' ');

  return html`
    <div ref=${shellRef} class="shell">
      <${IconSprite} />
      ${booting
        ? html`
            <div class="app standalone">
              <header class="top"><h1>Machines</h1></header>
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
                  setPairNotice(next?.last_paired || null);
                  hydrate(next);
                }}
              />
            `
          : html`
              <div class=${appClasses}>
                ${renderHeader()}
                <main
                  ref=${mainRef}
                  class=${isPhoneCommand ? 'main command-stream' : 'main'}
                  onPointerDown=${onMainPointerDown}
                  onPointerMove=${onMainPointerMove}
                  onPointerUp=${onMainPointerUp}
                  onPointerCancel=${onMainPointerCancel}
                >
                  ${pullState
                    ? html`
                        <div class="pull-hint" role="status">
                          ${pullState === 'refreshing'
                            ? 'Refreshing…'
                            : pullState === 'release'
                              ? 'Release to refresh'
                              : 'Pull to refresh'}
                        </div>
                      `
                    : null}
                  ${route.section === 'machines'
                    ? html`
                        <${MachinesView}
                          snapshot=${snapshot}
                          error=${fleetError}
                          pairNotice=${pairNotice}
                          onDismissPairNotice=${() => setPairNotice(null)}
                          loading=${refreshing}
                          wide=${wide}
                          route=${route}
                          link=${link}
                          nodeInfo=${nodeInfo}
                          tools=${tools}
                          allowedSet=${allowedSet}
                          settings=${settings}
                          wakePendingSet=${wakePendingSet}
                          onWakeMachine=${onWakeMachine}
                          onWakeSent=${markWakeSent}
                          onRequestConfirm=${openConfirm}
                          onRefresh=${() => {
                            refreshFleet(nodeInfo, { forceOffline: true });
                            triggerLearnInBackground();
                          }}
                          commandRuns=${{ byMachine: commandRunsByMachine, draft: commandDraft }}
                          onCommandRunsChange=${commandRunsApi}
                        />
                      `
                    : route.section === 'activity'
                      ? html`
                          <${ActivityView}
                            route=${route}
                            snapshot=${snapshot}
                            link=${link}
                            nodeInfo=${nodeInfo}
                            allowedSet=${allowedSet}
                            settings=${settings}
                            wide=${wide}
                            onRefreshFleet=${() => refreshFleet(nodeInfo)}
                          />
                        `
                      : route.section === 'actions'
                        ? html`
                            <${ActionsView}
                              route=${route}
                              tools=${tools}
                              allowedSet=${allowedSet}
                              nodeInfo=${nodeInfo}
                              snapshot=${snapshot}
                              link=${link}
                              settings=${settings}
                              wide=${wide}
                              query=${actionsQuery}
                              onWakeSent=${markWakeSent}
                              onRequestConfirm=${openConfirm}
                              onRefreshFleet=${() => refreshFleet(nodeInfo)}
                            />
                          `
                        : html`
                            <${SettingsView}
                              route=${route}
                              wide=${wide}
                              deviceNoun=${deviceNoun}
                              link=${link}
                              linkState=${linkState}
                              nodeInfo=${nodeInfo}
                              tools=${tools}
                              allowedSet=${allowedSet}
                              snapshot=${snapshot}
                              settings=${settings}
                              pairNotice=${pairNotice}
                              onDismissPairNotice=${() => setPairNotice(null)}
                              onUpdateSettings=${updateSettings}
                              onRequestConfirm=${openConfirm}
                              onAcceptIdentityChange=${handleAcceptIdentityChange}
                              onConnected=${(next) => {
                                setFleetError(null);
                                setPairNotice(next?.last_paired || null);
                                hydrate(next);
                              }}
                              onSelectNode=${
                                link.kind === 'app' && typeof link.selectNode === 'function'
                                  ? async (node) => {
                                      if (refreshAbortRef.current) {
                                        refreshAbortRef.current.abort();
                                        refreshAbortRef.current = null;
                                      }
                                      refreshInFlightRef.current = false;
                                      sessionCache.clear();
                                      persistentCache.clear();
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
                ${renderDock()}
                ${renderNav()}
              </div>
            `}
      ${confirmRequest
        ? html`
            <${ConfirmSheet}
              request=${confirmRequest}
              technical=${Boolean(settings.technical)}
              onClose=${closeConfirm}
            />
          `
        : null}
    </div>
  `;
}

render(html`<${App} />`, document.getElementById('root'));
