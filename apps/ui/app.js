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
import { createCapabilitiesStore } from './lib/capabilities.js';
import { containsTechnicalText, isTerminalHint, plainErrorMessage } from './lib/errors.js';
import {
  hydrateFleetCache,
  refreshFleetProgressive,
  serializeFleetCache,
} from './lib/fleet.js';
import {
  filesFallbackBackRoute,
  folderDisplayName,
  parentFilePath,
  selectAllEntries,
  selectionSummary,
  toggleSelection,
} from './lib/files.js';
import { readSortPreference, writeSortPreference } from './lib/files-store.js';
import { createFolderChangesStore } from './lib/folder-changes.js';
import { formatBytes, formatRelativeTime } from './lib/format.js';
import {
  isLocalConnectionName,
  isPendingLocalName,
  isRealMachineName,
  LOCAL_PIPE_CONNECTION,
  migrateLocalMachineStorage,
  PENDING_LOCAL_MACHINE_NAME,
} from './lib/local-machine.js';
import { createPairingController } from './lib/pairing.js';
import { createForegroundResumeController } from './lib/resume.js';
import { createThumbCache } from './lib/thumb-cache.js';
import {
  classifyHorizontalSwipe,
  EDGE_BACK_ZONE_PX,
  hasNativeEdgeBack,
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
import {
  applySettingsToRoot,
  detectDeviceNoun,
  readSettings,
  writeSettings,
} from './lib/settings.js';
import {
  ERROR_RETRY_INTERVAL_MS,
  PERIODIC_CHECK_INTERVAL_MS,
  countActiveTransfers,
  isSafeForAutoInstall,
  shouldCheckOnResume,
} from './lib/updates.js';
import { createWakeController, setDefaultWakeController } from './lib/wake.js';
import { createLink } from './link/index.js';
import { ActionsView } from './views/actions.js';
import { ActivityView } from './views/activity.js';
import { Notice, Sheet, WakeSheet } from './views/common.js';
import { FilesDock, FilesView } from './views/files.js';
import { CommandComposer } from './views/machine.js';
import { MachinesView } from './views/machines.js';
import { ScrollContainer } from './views/scroll-container.js';
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

function safeSessionStorage() {
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

const WAKE_TTL_MS = 120_000;
const PASSIVE_REFRESH_TOOLS = new Set([
  'fleet.list',
  'fleet.ping',
  'sys.info',
  'sys.procs',
  'power.status',
  'job.list',
  'meta.whoami',
  'meta.version',
  'fs.list',
  'fs.stat',
  'fs.read',
  'fs.search',
  'fs.tags',
  'fs.thumb',
  'fav.list',
  'sync.list',
]);

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
  const plainRefusalDetailMsg =
    refusalParts?.detailMessage && !containsTechnicalText(refusalParts.detailMessage)
      ? refusalParts.detailMessage
      : null;
  const plainRefusalDetailHint =
    refusalParts?.detailHint &&
    !isTerminalHint(refusalParts.detailHint) &&
    !containsTechnicalText(refusalParts.detailHint)
      ? refusalParts.detailHint
      : null;
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

      ${dryError ? html`<${Notice} error=${dryError} technical=${technical} />` : null}
      ${realError ? html`<${Notice} error=${realError} technical=${technical} />` : null}

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
                        ${plainRefusalDetailMsg || plainRefusalDetailHint
                          ? html`
                              <details class="more">
                                <summary>More</summary>
                                ${plainRefusalDetailMsg
                                  ? html`<p>${plainRefusalDetailMsg}</p>`
                                  : null}
                                ${plainRefusalDetailHint
                                  ? html`<p>${plainRefusalDetailHint}</p>`
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
  const rawLink = useMemo(() => createLink(), []);
  const activeCallsRef = useRef(0);
  const link = useMemo(() => {
    if (typeof rawLink.call !== 'function') return rawLink;
    return {
      ...rawLink,
      async call(tool, args, options) {
        const isInteractive = Boolean(options?.onEvent) || !PASSIVE_REFRESH_TOOLS.has(tool);
        if (isInteractive) activeCallsRef.current += 1;
        try {
          return await rawLink.call(tool, args, options);
        } finally {
          if (isInteractive) activeCallsRef.current = Math.max(0, activeCallsRef.current - 1);
        }
      },
    };
  }, [rawLink]);
  const sessionCache = useMemo(() => createSessionCache(), []);
  const persistentCache = useMemo(() => createPersistentCache(), []);
  const thumbCache = useMemo(() => createThumbCache({ maxEntries: 180, storage: null }), []);
  const capsStore = useMemo(() => createCapabilitiesStore(link), [link]);
  const folderChanges = useMemo(() => createFolderChangesStore(), []);
  useEffect(() => () => capsStore.dispose(), [capsStore]);
  useEffect(() => () => folderChanges.dispose(), [folderChanges]);

  // Reset any leftover native immersive or screen-on state once on page boot / reload.
  useEffect(() => {
    Promise.resolve(link?.immersive?.(false)).catch(() => {});
    Promise.resolve(link?.keepScreenOn?.(false)).catch(() => {});
  }, [link]);
  const shellRef = useRef(null);
  const mainRef = useRef(null);
  const refreshAbortRef = useRef(null);
  const refreshInFlightRef = useRef(false);
  const resumingRef = useRef(false);
  const screenRefreshRef = useRef(null);
  const popStateInterceptorRef = useRef(null);
  const swipeRef = useRef(null);
  const touchSwipeRef = useRef(null);
  const scrollByFolderRef = useRef(new Map());
  const pendingScrollRestoreKeyRef = useRef(null);
  const filesHistoryDepthRef = useRef(0);
  const viewerHistoryPushedRef = useRef(false);
  const pendingReplaceAfterPopRef = useRef(null);
  const filesSheetHistoryRef = useRef(false);
  const filesSelectHistoryRef = useRef(false);
  const lastHashRef = useRef(window.location.hash);
  const filesBulkHandlersRef = useRef({});
  const toastTimerRef = useRef(null);
  const toastSeqRef = useRef(0);
  const toastRef = useRef(null);
  const deviceTransfersRef = useRef([]);
  const prevTransferStatesRef = useRef(null);
  const activeTransferJobsRef = useRef(new Map());
  const seenTransferJobsRef = useRef(new Map());
  const [activeTransferJobCount, setActiveTransferJobCount] = useState(0);

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
  const [tools, setTools] = useState([]);
  const [snapshot, setSnapshot] = useState(null);
  const [fleetError, setFleetError] = useState(null);
  const [refreshing, setRefreshing] = useState(false);
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
  const [updateState, setUpdateState] = useState(null);
  const [whatIsNewOpen, setWhatIsNewOpen] = useState(false);
  const [startupUpdating, setStartupUpdating] = useState(false);
  const [wakeSentAt, setWakeSentAt] = useState({});
  const [wakeUiState, setWakeUiState] = useState({ sheet: null, wakingMachines: new Set() });
  const [toast, setToast] = useState(null);
  const [deviceTransfers, setDeviceTransfers] = useState([]);
  const [filesSortPref, setFilesSortPref] = useState(() => readSortPreference(safeLocalStorage()));
  const [filesKindFilter, setFilesKindFilter] = useState('');
  const [filesShowHidden, setFilesShowHidden] = useState(false);
  const [filesSelecting, setFilesSelecting] = useState(false);
  const [filesSelectedPaths, setFilesSelectedPaths] = useState(() => new Set());
  const [filesSheet, setFilesSheet] = useState(null);
  const [filesViewModeOverride, setFilesViewModeOverride] = useState(null);
  const [filesResolvedViewMode, setFilesResolvedViewMode] = useState('list');
  const [filesMeta, setFilesMeta] = useState({ caps: null, resolvedPath: '~', entries: [], syncFolders: [] });
  const [filesSearchSummary, setFilesSearchSummary] = useState({ count: 0, machineCount: 0, searching: false });
  const sheetHistoryRef = useRef(false);
  const learnInFlightRef = useRef(false);
  const updateCheckInFlightRef = useRef(false);
  const userInteractedRef = useRef(false);
  const firstScreenShownRef = useRef(false);
  const startupAutoInstallRanRef = useRef(false);
  const updateStateRef = useRef(updateState);
  updateStateRef.current = updateState;
  const confirmRequestRef = useRef(confirmRequest);
  confirmRequestRef.current = confirmRequest;
  const whatIsNewOpenRef = useRef(whatIsNewOpen);
  whatIsNewOpenRef.current = whatIsNewOpen;
  const commandDraftRef = useRef(commandDraft);
  commandDraftRef.current = commandDraft;
  const pairingOpenRef = useRef(pairingOpen);
  pairingOpenRef.current = pairingOpen;
  const filesSheetRef = useRef(filesSheet);
  filesSheetRef.current = filesSheet;
  const wakeSheetOpenRef = useRef(false);
  wakeSheetOpenRef.current = Boolean(wakeUiState?.sheet);
  const activeTransfersCountRef = useRef(0);
  activeTransfersCountRef.current = countActiveTransfers(deviceTransfers);

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
  const allowedSetRef = useRef(allowedSet);
  allowedSetRef.current = allowedSet;
  const wakeControllerRef = useRef(null);

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
    if (document.documentElement?.dataset) {
      document.documentElement.dataset.deviceNoun = deviceNoun;
    }
  }, [settings, deviceNoun]);

  const routeRef = useRef(route);
  routeRef.current = route;
  toastRef.current = toast;
  deviceTransfersRef.current = deviceTransfers;

  const isTerminalTransferState = (state) => state === 'done' || state === 'failed' || state === 'cancelled';

  const allTrackedTransfersTerminal = (ids, list) => {
    if (!Array.isArray(ids) || ids.length === 0) return false;
    const byId = new Map((list || []).map((t) => [t.id, t.state]));
    return ids.every((id) => isTerminalTransferState(byId.get(id)));
  };

  const formatTransferFailureToast = (failedList) => {
    if (!Array.isArray(failedList) || failedList.length === 0) return '';
    if (failedList.length === 1) {
      const item = failedList[0];
      const verb = item.direction === 'upload' ? 'Upload' : 'Download';
      const name = item.name || 'file';
      const errObj =
        item.error && typeof item.error === 'object'
          ? { ...item.error, machine: item.error.machine || item.machine || null }
          : item.error;
      const detailText = errObj
        ? plainErrorMessage(errObj, '', { technical: Boolean(settingsRef.current?.technical), deviceNoun })
        : '';
      const detail = detailText ? ` ${detailText}` : '';
      return `${verb} of ${name} failed.${detail}`;
    }
    return `${failedList.length} transfers failed.`;
  };

  const clearToast = useCallback(() => {
    if (toastTimerRef.current) {
      clearTimeout(toastTimerRef.current);
      toastTimerRef.current = null;
    }
    toastRef.current = null;
    setToast(null);
  }, []);

  useEffect(() => () => {
    if (toastTimerRef.current) {
      clearTimeout(toastTimerRef.current);
      toastTimerRef.current = null;
    }
  }, []);

  const registerScreenRefresh = useCallback((fn) => {
    if (typeof fn !== 'function') return () => {};
    screenRefreshRef.current = fn;
    return () => {
      if (screenRefreshRef.current === fn) {
        screenRefreshRef.current = null;
      }
    };
  }, []);

  const observeTransferJobs = useCallback(
    (jobs) => {
      folderChanges.observeTransferJobs(jobs, seenTransferJobsRef.current);
    },
    [folderChanges],
  );

  const trackTransferJobs = useCallback((jobs, toastSeq = null) => {
    const clean = Array.isArray(jobs) ? jobs.filter((j) => j && j.id) : [];
    if (clean.length === 0) return;
    let added = false;
    for (const job of clean) {
      const key = `${job.machine || ''}:${job.id}`;
      activeTransferJobsRef.current.set(key, {
        ...job,
        toastSeq,
        startedAt: Date.now(),
      });
      if (!seenTransferJobsRef.current.has(key)) {
        seenTransferJobsRef.current.set(key, { state: 'running', filesDone: 0 });
      }
      added = true;
    }
    if (added) {
      setActiveTransferJobCount(activeTransferJobsRef.current.size);
    }
  }, []);

  const showToast = useCallback(
    (message, opts = {}) => {
      if (!message) return;
      const actionHref = opts.actionHref || null;
      const suppressOnActivity =
        opts.suppressOnActivity !== undefined
          ? Boolean(opts.suppressOnActivity)
          : actionHref === '#/activity';
      const transferJobs = Array.isArray(opts.transferJobs)
        ? opts.transferJobs.filter((j) => j && j.id)
        : [];
      const seq = ++toastSeqRef.current;

      // Always track background transfer jobs for live folder refresh, even if the toast
      // is suppressed on Activity or later dismissed.
      if (transferJobs.length > 0) {
        trackTransferJobs(transferJobs, seq);
      }
      if (opts.transferJobsPromise && typeof opts.transferJobsPromise.then === 'function') {
        opts.transferJobsPromise
          .then((jobs) => {
            const clean = Array.isArray(jobs) ? jobs.filter((j) => j && j.id) : [];
            if (clean.length === 0) return;
            trackTransferJobs(clean, seq);
            if (toastRef.current?.seq !== seq) return;
            const updated = { ...toastRef.current, transferJobs: clean };
            toastRef.current = updated;
            setToast(updated);
          })
          .catch(() => {});
      }

      if (suppressOnActivity && routeRef.current?.section === 'activity') {
        return;
      }
      const transferIds = Array.isArray(opts.transferIds) ? opts.transferIds.filter(Boolean) : [];
      if (transferIds.length > 0 && allTrackedTransfersTerminal(transferIds, deviceTransfersRef.current)) {
        return;
      }
      if (toastTimerRef.current) {
        clearTimeout(toastTimerRef.current);
        toastTimerRef.current = null;
      }
      const tracksTransfer =
        transferIds.length > 0 ||
        transferJobs.length > 0 ||
        Boolean(opts.transferIdsPromise) ||
        Boolean(opts.transferJobsPromise);
      const nextToast = {
        seq,
        message,
        actionLabel: opts.actionLabel || null,
        actionHref,
        onAction: typeof opts.onAction === 'function' ? opts.onAction : null,
        tone: opts.tone || null,
        suppressOnActivity,
        transferIds,
        transferJobs,
      };
      toastRef.current = nextToast;
      setToast(nextToast);
      const ttlMs =
        typeof opts.ttlMs === 'number'
          ? opts.ttlMs
          : tracksTransfer || nextToast.onAction
            ? 8000
            : opts.tone === 'error'
              ? 6000
              : 4500;
      toastTimerRef.current = window.setTimeout(() => {
        toastTimerRef.current = null;
        if (toastRef.current?.seq === seq) {
          toastRef.current = null;
          setToast(null);
        }
      }, ttlMs);

      if (opts.transferIdsPromise && typeof opts.transferIdsPromise.then === 'function') {
        opts.transferIdsPromise
          .then((ids) => {
            const clean = Array.isArray(ids) ? ids.filter(Boolean) : [];
            if (clean.length === 0 || toastRef.current?.seq !== seq) return;
            if (allTrackedTransfersTerminal(clean, deviceTransfersRef.current)) {
              clearToast();
              return;
            }
            const updated = { ...toastRef.current, transferIds: clean };
            toastRef.current = updated;
            setToast(updated);
          })
          .catch(() => {});
      }
    },
    [clearToast, trackTransferJobs],
  );

  useEffect(() => {
    if (
      route.section === 'activity' &&
      (toastRef.current?.suppressOnActivity || toastRef.current?.actionHref === '#/activity')
    ) {
      clearToast();
    }
  }, [route.section, clearToast]);

  // Keep device transfer queue in sync when the link supports it.
  useEffect(() => {
    prevTransferStatesRef.current = null;
    if (!linkState?.ready || typeof link.transfers !== 'function') {
      deviceTransfersRef.current = [];
      setDeviceTransfers([]);
      return undefined;
    }
    let alive = true;
    const applyTransfers = (list) => {
      if (!alive || !Array.isArray(list)) return;
      const prevStates = prevTransferStatesRef.current;
      const nextStates = new Map(list.map((t) => [t.id, t.state]));
      const newlyFailed = [];
      if (prevStates !== null) {
        for (const item of list) {
          const prev = prevStates.get(item.id);
          if (item.state === 'failed' && prev !== 'failed') {
            newlyFailed.push(item);
          }
        }
        folderChanges.observeDeviceTransfers(list, prevStates);
      }
      prevTransferStatesRef.current = nextStates;
      deviceTransfersRef.current = list;
      setDeviceTransfers(list);

      const currentToast = toastRef.current;
      if (
        currentToast?.transferIds?.length > 0 &&
        allTrackedTransfersTerminal(currentToast.transferIds, list)
      ) {
        clearToast();
      }

      if (newlyFailed.length > 0) {
        const onActivity = routeRef.current?.section === 'activity';
        showToast(formatTransferFailureToast(newlyFailed), {
          tone: 'error',
          suppressOnActivity: false,
          ...(onActivity ? {} : { actionLabel: 'Activity', actionHref: '#/activity' }),
        });
      }
    };
    link
      .transfers()
      .then(applyTransfers)
      .catch(() => {});
    const unsub =
      typeof link.onTransferEvent === 'function'
        ? link.onTransferEvent(applyTransfers)
        : null;
    return () => {
      alive = false;
      if (typeof unsub === 'function') unsub();
    };
  }, [link, linkState?.ready, clearToast, showToast, folderChanges]);

  // Track background fs.transfer jobs until they finish — even minutes after the toast closes —
  // so destination (and source, on move) folders refresh live as files arrive and when done.
  useEffect(() => {
    if (activeTransferJobCount <= 0 || !linkState?.ready) {
      return undefined;
    }
    let alive = true;
    let timer = null;
    const isTerminalJob = (s) => s === 'succeeded' || s === 'failed' || s === 'cancelled';

    const checkJobs = async () => {
      if (!alive) return;
      const entries = Array.from(activeTransferJobsRef.current.entries());
      if (entries.length === 0) {
        setActiveTransferJobCount(0);
        return;
      }

      const statuses = await Promise.all(
        entries.map(([key, job]) =>
          link
            .call('job.status', { id: job.id }, { on: job.machine || nodeInfoRef.current?.node })
            .then((st) => ({ key, job, status: st }))
            .catch(() => ({ key, job, status: null })),
        ),
      );
      if (!alive) return;

      const failed = [];
      const completedToastSeqs = new Set();

      for (const { key, job, status } of statuses) {
        if (!status) continue;
        const normalized = {
          ...status,
          kind: status.kind || 'transfer',
          machine: job.machine || status.machine || nodeInfoRef.current?.node,
          move: status.move ?? job.move,
          destMachine: job.destMachine,
          srcMachine: job.srcMachine,
        };
        const prevSeen = seenTransferJobsRef.current.get(key);
        folderChanges.observeTransferJobs([normalized], seenTransferJobsRef.current);

        // Fallback if a minimal node's job.status omits `to` / `from`: use our tracked destination.
        const justFinished = status.state === 'succeeded' && prevSeen?.state !== 'succeeded';
        if (justFinished && !status.to && job.destMachine && job.destDir) {
          folderChanges.changed(job.destMachine, job.destDir, { throttle: false, reason: 'transfer' });
          if (job.move && job.srcMachine && job.srcDir) {
            folderChanges.changed(job.srcMachine, job.srcDir, { throttle: false, reason: 'transfer' });
          }
        } else if (
          !justFinished &&
          !status.to &&
          (status.state === 'running' || status.state === 'starting') &&
          (status.progress?.files_done || 0) > (prevSeen?.filesDone || 0) &&
          job.destMachine &&
          job.destDir
        ) {
          folderChanges.changed(job.destMachine, job.destDir, {
            throttle: true,
            reason: 'transfer-progress',
          });
        }

        if (isTerminalJob(status.state)) {
          activeTransferJobsRef.current.delete(key);
          if (job.toastSeq != null) completedToastSeqs.add(job.toastSeq);
          if (status.state === 'failed') failed.push(job);
        }
      }

      if (activeTransferJobsRef.current.size !== entries.length) {
        setActiveTransferJobCount(activeTransferJobsRef.current.size);
      }

      const curToast = toastRef.current;
      if (curToast && completedToastSeqs.has(curToast.seq)) {
        const stillForToast = Array.from(activeTransferJobsRef.current.values()).some(
          (j) => j.toastSeq === curToast.seq,
        );
        if (!stillForToast) clearToast();
      }

      if (failed.length > 0) {
        const onActivity = routeRef.current?.section === 'activity';
        const first = failed[0];
        const msg =
          failed.length === 1
            ? `${first.move ? 'Move' : 'Copy'} of ${first.label || 'item'} to ${first.destMachine || 'machine'} failed.`
            : `${failed.length} transfers failed.`;
        showToast(msg, {
          tone: 'error',
          suppressOnActivity: false,
          ...(onActivity ? {} : { actionLabel: 'Activity', actionHref: '#/activity' }),
        });
      }

      if (activeTransferJobsRef.current.size > 0 && alive) {
        const oldest = Math.min(
          ...Array.from(activeTransferJobsRef.current.values()).map((j) => j.startedAt || Date.now()),
        );
        const delayMs = Date.now() - oldest > 15_000 ? 1500 : 700;
        timer = window.setTimeout(checkJobs, delayMs);
      }
    };

    timer = window.setTimeout(checkJobs, 700);
    return () => {
      alive = false;
      if (timer) window.clearTimeout(timer);
    };
  }, [activeTransferJobCount, link, linkState?.ready, clearToast, showToast, folderChanges]);

  // Also observe transfer jobs discovered through fleet refreshes.
  useEffect(() => {
    if (!snapshot?.machines) return;
    const allJobs = [];
    for (const m of snapshot.machines) {
      if (!m?.name || !Array.isArray(m.jobs)) continue;
      for (const j of m.jobs) {
        if (j && j.kind === 'transfer') {
          allJobs.push({ ...j, machine: m.name });
        }
      }
    }
    if (allJobs.length > 0) {
      folderChanges.observeTransferJobs(allJobs, seenTransferJobsRef.current);
    }
  }, [snapshot, folderChanges]);

  useEffect(() => {
    if (mainRef.current && route.section !== 'files') mainRef.current.scrollTop = 0;
  }, [route.section]);

  useEffect(() => {
    if (!wide && mainRef.current && route.tab !== 'command' && route.section !== 'files') {
      mainRef.current.scrollTop = 0;
    }
  }, [wide, route.section, route.machine, route.tab, route.tool, route.action, route.jobId, route.page, route.sub]);

  // Mark pending scroll restore only when navigating to a different Files folder (never on live refresh).
  useEffect(() => {
    if (route.section !== 'files' || route.view !== 'folder' || route.viewer) {
      pendingScrollRestoreKeyRef.current = null;
      return;
    }
    pendingScrollRestoreKeyRef.current = `${route.machine || ''}\0${route.path || '~'}`;
  }, [route.section, route.view, route.machine, route.path, route.viewer]);

  // Restore saved folder scroll position once when arriving at a Files folder (e.g. via Back).
  useEffect(() => {
    if (route.section !== 'files' || route.view !== 'folder' || route.viewer) return;
    const key = `${route.machine || ''}\0${route.path || '~'}`;
    if (pendingScrollRestoreKeyRef.current !== key) return;
    const saved = scrollByFolderRef.current.get(key);
    if (typeof saved === 'number' && saved > 0 && filesMeta.entries.length === 0) {
      return;
    }
    pendingScrollRestoreKeyRef.current = null;
    if (mainRef.current) {
      const target = typeof saved === 'number' ? saved : 0;
      window.requestAnimationFrame(() => {
        if (mainRef.current) mainRef.current.scrollTop = target;
      });
    }
  }, [route.section, route.view, route.machine, route.path, route.viewer, filesMeta.entries.length]);

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
      lastHashRef.current = window.location.hash;
      const prevRoute = routeRef.current;
      if (wakeControllerRef.current) {
        wakeControllerRef.current.leaveScreen();
      }
      if (prevRoute?.section === 'files' && prevRoute.view === 'folder' && !prevRoute.viewer && mainRef.current) {
        scrollByFolderRef.current.set(`${prevRoute.machine || ''}\0${prevRoute.path || '~'}`, mainRef.current.scrollTop);
      }
      const next = parseRoute(window.location.hash);
      if (next.section !== 'files') {
        filesHistoryDepthRef.current = 0;
        viewerHistoryPushedRef.current = false;
      } else if (!next.viewer) {
        viewerHistoryPushedRef.current = false;
      } else if (window.history.state?.legionViewerOverlay) {
        viewerHistoryPushedRef.current = true;
      }
      if (
        (prevRoute?.section === 'files' && (next.section !== 'files' || next.machine !== prevRoute.machine || next.path !== prevRoute.path)) ||
        (prevRoute?.section === 'machines' && (next.section !== 'machines' || next.machine !== prevRoute.machine || next.tab !== prevRoute.tab))
      ) {
        setFilesViewModeOverride(null);
        setFilesKindFilter('');
      }
      setRoute((prev) => {
        if (mainRef.current && next.section !== 'files' && (prev.section !== next.section || !wideRef.current)) {
          mainRef.current.scrollTop = 0;
        }
        return next;
      });
    }
    function onPopState() {
      if (pendingReplaceAfterPopRef.current) {
        const nextHref = pendingReplaceAfterPopRef.current;
        pendingReplaceAfterPopRef.current = null;
        window.history.replaceState(null, '', nextHref);
        syncRoute();
        return;
      }
      const st = window.history.state;
      if (sheetHistoryRef.current && !st?.legionSheet) {
        sheetHistoryRef.current = false;
        setConfirmRequest(null);
        return;
      }
      if (filesSheetHistoryRef.current && !st?.legionFilesSheet) {
        filesSheetHistoryRef.current = false;
        setFilesSheet(null);
        return;
      }
      if (filesSelectHistoryRef.current && !st?.legionFilesSelect) {
        filesSelectHistoryRef.current = false;
        setFilesSelecting(false);
        setFilesSelectedPaths(new Set());
        return;
      }
      if (popStateInterceptorRef.current?.()) {
        lastHashRef.current = window.location.hash;
        return;
      }
      if (window.location.hash !== lastHashRef.current) {
        if (filesHistoryDepthRef.current > 0) {
          filesHistoryDepthRef.current -= 1;
        }
        syncRoute();
      }
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
          reachable: n.reachable !== undefined ? n.reachable : old.reachable,
          whoamiStale: n.whoamiStale !== undefined ? n.whoamiStale : old.whoamiStale,
          roleCheckedAt: n.roleCheckedAt || n.role_checked_at || old.roleCheckedAt || old.role_checked_at || null,
          unreachableError: n.unreachableError !== undefined ? n.unreachableError : old.unreachableError,
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

  // Refresh link route and learn-rejection state whenever Settings -> Connections opens so
  // route changes caused by recent calls or streams (e.g. relay -> direct) appear immediately.
  useEffect(() => {
    if (link.kind !== 'app' || typeof link.state !== 'function') return;
    if (route.section !== 'settings' || route.page !== 'connections') return;
    let alive = true;
    link
      .state()
      .then((st) => {
        if (alive && st) mergeBackendState(st);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [link, route.section, route.page, mergeBackendState]);

  const checkUpdatesQuietly = useCallback(async () => {
    if (typeof link.checkUpdates !== 'function' || updateCheckInFlightRef.current) return;
    updateCheckInFlightRef.current = true;
    try {
      const next = await link.checkUpdates();
      if (next) setUpdateState(next);
    } catch {
      // A check that finds no machine reachable stays quiet.
    } finally {
      updateCheckInFlightRef.current = false;
    }
  }, [link]);

  const handleCheckUpdates = useCallback(async () => {
    if (typeof link.checkUpdates !== 'function') return null;
    updateCheckInFlightRef.current = true;
    try {
      const next = await link.checkUpdates();
      if (next) setUpdateState(next);
      return next;
    } finally {
      updateCheckInFlightRef.current = false;
    }
  }, [link]);

  const handleInstallUpdate = useCallback(async () => {
    if (typeof link.installUpdate !== 'function') return null;
    const promptConfirm = () => {
      if (!sheetHistoryRef.current) {
        window.history.pushState({ legionSheet: true }, '');
        sheetHistoryRef.current = true;
      }
      setConfirmRequest({
        title: 'Update and restart now?',
        summary:
          'Something is still running in Legion. Installing the update will restart the app and interrupt it.',
        confirmLabel: 'Update now',
        danger: false,
        async onConfirm() {
          const next = await link.installUpdate({ confirmInterrupt: true });
          if (next) setUpdateState(next);
        },
      });
    };
    if (
      commandDraftRef.current.running ||
      activeCallsRef.current > 0 ||
      activeTransfersCountRef.current > 0 ||
      resumingRef.current
    ) {
      promptConfirm();
      return null;
    }
    try {
      const next = await link.installUpdate();
      if (next) setUpdateState(next);
      return next;
    } catch (err) {
      if (err?.code === 'conflict') {
        promptConfirm();
        return null;
      }
      throw err;
    }
  }, [link]);

  const handleSetAutoUpdate = useCallback(
    async (enabled) => {
      if (typeof link.setAutoUpdate !== 'function') return null;
      const next = await link.setAutoUpdate(enabled);
      if (next) setUpdateState(next);
      return next;
    },
    [link],
  );

  const handleDismissUpdateNotice = useCallback(async () => {
    setWhatIsNewOpen(false);
    if (typeof link.dismissUpdateNotice !== 'function') return;
    try {
      const next = await link.dismissUpdateNotice();
      if (next) setUpdateState(next);
    } catch {
      // Ignore dismiss failure; next load will re-read state.
    }
  }, [link]);

  const applyLocalNodeMigration = useCallback(
    (realName) => {
      if (!isRealMachineName(realName)) return;
      migrateLocalMachineStorage(realName, {
        localStorage: safeLocalStorage(),
        sessionStorage: safeSessionStorage(),
      });
      capsStore.renameMachine(LOCAL_PIPE_CONNECTION, realName);
      capsStore.renameMachine(PENDING_LOCAL_MACHINE_NAME, realName);
      folderChanges.renameMachine(LOCAL_PIPE_CONNECTION, realName);
      folderChanges.renameMachine(PENDING_LOCAL_MACHINE_NAME, realName);
      thumbCache.renameMachine(LOCAL_PIPE_CONNECTION, realName);
      thumbCache.renameMachine(PENDING_LOCAL_MACHINE_NAME, realName);

      const cur = routeRef.current;
      if (cur) {
        let changed = false;
        const nextRoute = { ...cur };
        if (isLocalConnectionName(cur.machine) || isPendingLocalName(cur.machine)) {
          nextRoute.machine = realName;
          changed = true;
        }
        if (
          cur.viewer &&
          (isLocalConnectionName(cur.viewer.machine) || isPendingLocalName(cur.viewer.machine))
        ) {
          nextRoute.viewer = { ...cur.viewer, machine: realName };
          changed = true;
        }
        if (
          cur.section === 'settings' &&
          cur.page === 'connections' &&
          cur.sub === 'node' &&
          (isLocalConnectionName(cur.item) || isPendingLocalName(cur.item))
        ) {
          nextRoute.item = realName;
          changed = true;
        }
        if (changed) {
          const nextHref = routeHref(nextRoute);
          if (nextHref && nextHref !== window.location.hash) {
            window.history.replaceState(window.history.state, '', nextHref);
            lastHashRef.current = nextHref;
          }
          setRoute(nextRoute);
        }
      }
    },
    [capsStore, folderChanges, thumbCache],
  );

  const handleLocalNodeResolved = useCallback(
    (realName) => {
      if (!isRealMachineName(realName)) return;
      if (typeof link.setLocalMachineName === 'function') {
        link.setLocalMachineName(realName);
      }
      applyLocalNodeMigration(realName);
      setLinkState((prev) => {
        if (!prev || !Array.isArray(prev.nodes)) return prev;
        const isSelfName = (name) =>
          name === realName || isLocalConnectionName(name) || isPendingLocalName(name);
        const mapLocalHop = (name) =>
          isLocalConnectionName(name) || isPendingLocalName(name) ? realName : name;
        const indexByName = new Map();
        const nextNodes = [];
        for (const n of prev.nodes) {
          if (!n) continue;
          const isLocalEntry =
            n.isLocalComputer ||
            n.nodeNamePending ||
            n.via === LOCAL_PIPE_CONNECTION ||
            isLocalConnectionName(n.node) ||
            isPendingLocalName(n.node);
          const nodeName = isLocalEntry ? realName : n.node;
          if (!nodeName) continue;
          const entry = isLocalEntry
            ? {
                ...n,
                node: realName,
                via: LOCAL_PIPE_CONNECTION,
                route: 'direct',
                route_via: null,
                learned_from: null,
                identity_changed: null,
                isLocalComputer: true,
                nodeNamePending: false,
                reachable: true,
                whoami: n.whoami ? { ...n.whoami, node: realName } : n.whoami,
              }
            : {
                ...n,
                route_via: mapLocalHop(n.route_via),
                learned_from: mapLocalHop(n.learned_from),
              };
          if (indexByName.has(nodeName)) {
            if (isLocalEntry) nextNodes[indexByName.get(nodeName)] = entry;
            continue;
          }
          indexByName.set(nodeName, nextNodes.length);
          nextNodes.push(entry);
        }
        let nextRoutes = prev.routes;
        if (prev.routes && typeof prev.routes === 'object') {
          const hadLocal = Object.keys(prev.routes).some((k) => isSelfName(k));
          const rebuilt = {};
          for (const [k, info] of Object.entries(prev.routes)) {
            if (!k || isSelfName(k)) continue;
            rebuilt[k] =
              info && typeof info === 'object'
                ? { ...info, via: mapLocalHop(info.via), introducer: mapLocalHop(info.introducer) }
                : info;
          }
          if (hadLocal) {
            rebuilt[realName] = { kind: 'direct', via: null, introducer: null };
          }
          nextRoutes = rebuilt;
        }
        const nextRejections = Array.isArray(prev.learn_rejections)
          ? prev.learn_rejections
              .filter((r) => r && !isSelfName(r.machine))
              .map((r) => ({ ...r, reported_by: mapLocalHop(r.reported_by) }))
          : prev.learn_rejections;
        const nextDisagreements = Array.isArray(prev.disagreements)
          ? prev.disagreements
              .filter((d) => d && !isSelfName(d.machine))
              .map((d) => ({
                ...d,
                reports: Array.isArray(d.reports)
                  ? d.reports.map((r) => (r ? { ...r, reported_by: mapLocalHop(r.reported_by) } : r))
                  : d.reports,
              }))
          : prev.disagreements;
        const nextState = {
          ...prev,
          nodes: nextNodes,
          routes: nextRoutes,
          learn_rejections: nextRejections,
          disagreements: nextDisagreements,
        };
        nodeInfoRef.current = nextNodes[0] || null;
        return nextState;
      });
      capsStore
        .ensure(realName)
        .then((entry) => {
          const toolList = entry?.tools;
          if (Array.isArray(toolList) && toolList.length > 0 && nodeInfoRef.current?.node === realName) {
            setTools(toolList);
            sessionCache.set(`tools:${realName}`, toolList);
          }
        })
        .catch(() => {});
    },
    [link, applyLocalNodeMigration, capsStore, sessionCache],
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
          isLocalComputer: Boolean(targetNode.isLocalComputer),
          nodeNamePending: Boolean(targetNode.nodeNamePending),
          initialMachines: baseSnap?.machines || [],
          cachedAt: baseSnap?.cachedAt ?? null,
          forceOffline: Boolean(options.forceOffline),
          checkOffline: settingsRef.current.checkOffline !== false,
          signal: ctrl.signal,
          onLocalNodeResolved: handleLocalNodeResolved,
          onUpdate(next) {
            if (ctrl.signal.aborted) return;
            setSnapshot(next);
          },
          onSettled(finalSnap) {
            if (ctrl.signal.aborted) return;
            if (link.kind !== 'mock') {
              const activeName = nodeInfoRef.current?.node || targetNode.node;
              if (isRealMachineName(activeName)) {
                persistentCache.set('fleet', serializeFleetCache(activeName, finalSnap.machines));
              }
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
    [link, persistentCache, handleLocalNodeResolved],
  );

  const handleAcceptIdentityChange = useCallback(
    async (nodeName, fingerprints) => {
      if (typeof link.acceptIdentityChange !== 'function') return;
      const next = await link.acceptIdentityChange(nodeName, fingerprints);
      if (next) mergeBackendState(next);
      refreshFleet(nodeInfoRef.current, { background: true });
    },
    [link, mergeBackendState, refreshFleet],
  );

  const handleDismissIdentityChange = useCallback(
    async (nodeName) => {
      if (typeof link.dismissIdentityChange !== 'function') return;
      const next = await link.dismissIdentityChange(nodeName);
      if (next) mergeBackendState(next);
    },
    [link, mergeBackendState],
  );

  const hydrate = useCallback(
    async (nextState) => {
      nodeInfoRef.current = nextState?.nodes?.[0] || null;
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
      if (isRealMachineName(primary.node)) {
        applyLocalNodeMigration(primary.node);
      }
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

      const toolsCacheKey = `tools:${primary.node}`;
      const cachedNodeTools = sessionCache.get(toolsCacheKey)?.value;
      setTools(Array.isArray(cachedNodeTools) ? cachedNodeTools : []);
      capsStore.syncMachineMeta(primary.node, {
        version: primary.version || null,
        build: primary.build || null,
        reachable: primary.reachable !== false,
      });
      if (primary.reachable !== false) {
        capsStore
          .ensure(primary.node)
          .then((entry) => {
            const toolList = entry?.tools;
            if (Array.isArray(toolList) && toolList.length > 0 && nodeInfoRef.current?.node === primary.node) {
              setTools(toolList);
              sessionCache.set(toolsCacheKey, toolList);
            }
          })
          .catch(() => {});
      }

      refreshFleet(primary, { initialSnapshot: cachedSnap });
      triggerLearnInBackground();
      if (!startupAutoInstallRanRef.current && updateStateRef.current?.phase !== 'ready') {
        checkUpdatesQuietly();
      }
    },
    [link, capsStore, sessionCache, persistentCache, applyLocalNodeMigration, refreshFleet, triggerLearnInBackground, checkUpdatesQuietly],
  );

  const linkReadyRef = useRef(Boolean(linkState?.ready));
  linkReadyRef.current = Boolean(linkState?.ready);
  const hydrateRef = useRef(hydrate);
  hydrateRef.current = hydrate;
  const showToastRef = useRef(showToast);
  showToastRef.current = showToast;
  const deviceNounRef = useRef(deviceNoun);
  deviceNounRef.current = deviceNoun;

  const pairingController = useMemo(() => {
    const ctrl = createPairingController({
      link,
      getDeviceNoun: () => deviceNounRef.current,
      isOnPairingScreen: () => {
        if (!linkReadyRef.current || pairingOpenRef.current) return true;
        const r = routeRef.current;
        return r?.section === 'settings' && r?.page === 'connections' && r?.sub === 'pair';
      },
      onPaired: ({ next, node, startedScreen, onPairingScreen }) => {
        setPairingOpen(false);
        setFleetError(null);
        capsStore.invalidate();
        folderChanges.clearCache();
        if (onPairingScreen) {
          if (startedScreen === 'settings') {
            window.location.hash = '#/settings/connections';
          } else {
            window.location.hash = '#/machines';
          }
          setPairNotice(
            next?.last_paired
              ? {
                  summary: next.last_paired,
                  screen: startedScreen === 'settings' ? 'connections' : 'machines',
                }
              : null,
          );
        }
        hydrateRef.current(next);
        showToastRef.current(node ? `Paired with ${node}` : 'Paired');
      },
      onBackgroundOutcome: ({ kind, message, error, retryable, restorable }) => {
        if (kind === 'already_paired') {
          showToastRef.current(message, {
            tone: 'error',
            actionLabel: restorable ? 'Restore pairing' : null,
            onAction: restorable ? () => ctrl.restorePair() : null,
            ttlMs: 12000,
          });
          return;
        }
        if (kind === 'uncertain' || kind === 'not_finished') {
          showToastRef.current(message, {
            tone: 'error',
            actionLabel: retryable ? 'Try again' : null,
            onAction: retryable ? () => ctrl.retryPair() : null,
            ttlMs: 12000,
          });
          return;
        }
        const summary = plainErrorMessage(error, 'Pairing failed.', {
          technical: Boolean(settingsRef.current?.technical),
          deviceNoun: deviceNounRef.current,
        });
        showToastRef.current(summary, {
          tone: 'error',
          actionLabel: retryable ? 'Try again' : null,
          onAction: retryable ? () => ctrl.retryPair() : null,
          ttlMs: 10000,
        });
      },
    });
    return ctrl;
  }, [link, capsStore, folderChanges]);

  const [pairingState, setPairingState] = useState(() => pairingController.getState());

  useEffect(() => {
    setPairingState(pairingController.getState());
    return pairingController.subscribe(setPairingState);
  }, [pairingController]);

  // Feed fleet version/build/reachability into the per-machine capabilities store so build
  // upgrades and reconnects invalidate cached tool schemas automatically.
  useEffect(() => {
    if (!snapshot?.machines) return;
    for (const m of snapshot.machines) {
      if (m?.name) {
        capsStore.syncMachineMeta(m.name, {
          version: m.version || null,
          build: m.build || null,
          reachable: m.reachable,
        });
      }
    }
  }, [capsStore, snapshot]);

  // Track any user interaction (including wheel/scroll) and subscribe to live updater state.
  // Startup auto-install runs once during boot before the first full screen renders — never on
  // subsequent live state pushes mid-session.
  useEffect(() => {
    const markInteracted = () => {
      userInteractedRef.current = true;
    };
    window.addEventListener('pointerdown', markInteracted, { passive: true, once: true });
    window.addEventListener('keydown', markInteracted, { passive: true, once: true });
    window.addEventListener('touchstart', markInteracted, { passive: true, once: true });
    window.addEventListener('wheel', markInteracted, { passive: true, once: true });
    window.addEventListener('scroll', markInteracted, { passive: true, capture: true, once: true });

    let alive = true;
    const unwatch =
      typeof link.watchUpdates === 'function'
        ? link.watchUpdates((st) => {
            if (!alive || !st) return;
            setUpdateState(st);
          })
        : () => {};

    return () => {
      alive = false;
      unwatch();
      window.removeEventListener('pointerdown', markInteracted);
      window.removeEventListener('keydown', markInteracted);
      window.removeEventListener('touchstart', markInteracted);
      window.removeEventListener('wheel', markInteracted);
      window.removeEventListener('scroll', markInteracted, { capture: true });
    };
  }, [link]);

  // Trigger periodic (6h) update checks while the app is open and visible, and retry sooner (5m)
  // after a transient check or download error.
  useEffect(() => {
    if (!linkState?.ready || typeof link.checkUpdates !== 'function') return;

    const periodicTimer = window.setInterval(() => {
      if (document.visibilityState === 'visible') {
        checkUpdatesQuietly();
      }
    }, PERIODIC_CHECK_INTERVAL_MS);

    return () => {
      window.clearInterval(periodicTimer);
    };
  }, [link, linkState?.ready, checkUpdatesQuietly]);

  useEffect(() => {
    if (
      !linkState?.ready ||
      typeof link.checkUpdates !== 'function' ||
      updateState?.phase !== 'error' ||
      updateState?.failed_update
    ) {
      return;
    }
    const retryTimer = window.setTimeout(() => {
      if (document.visibilityState === 'visible') {
        checkUpdatesQuietly();
      }
    }, ERROR_RETRY_INTERVAL_MS);
    return () => {
      window.clearTimeout(retryTimer);
    };
  }, [link, linkState?.ready, updateState?.phase, updateState?.failed_update, updateState?.last_checked_at, checkUpdatesQuietly]);

  useEffect(() => {
    let alive = true;
    (async () => {
      // Check for an already-downloaded update BEFORE calling link.state() / painting the first
      // full screen so startup auto-install runs only at launch with zero native calls in flight.
      if (
        typeof link.updateState === 'function' &&
        !startupAutoInstallRanRef.current &&
        !firstScreenShownRef.current
      ) {
        try {
          const initialUpdate = await link.updateState();
          if (!alive) return;
          if (initialUpdate) {
            setUpdateState(initialUpdate);
            if (
              typeof link.maybeAutoInstall === 'function' &&
              isSafeForAutoInstall({
                updateState: initialUpdate,
                trigger: 'startup',
                userInteracted: userInteractedRef.current,
                firstScreenShown: firstScreenShownRef.current,
                hasOpenSheet: Boolean(
                  confirmRequestRef.current ||
                    whatIsNewOpenRef.current ||
                    filesSheetRef.current ||
                    wakeSheetOpenRef.current,
                ),
                hasCommandDraft: Boolean(commandDraftRef.current.cmd?.trim()),
                isRunningCommand: Boolean(commandDraftRef.current.running),
                activeCalls: activeCallsRef.current + activeTransfersCountRef.current,
                isPairingOpen: Boolean(pairingOpenRef.current),
                isResuming: resumingRef.current,
              })
            ) {
              startupAutoInstallRanRef.current = true;
              const isMock = Boolean(link.mockFlags);
              if (!isMock) setStartupUpdating(true);
              try {
                const res = await link.maybeAutoInstall('startup');
                if (!alive) return;
                if (res) setUpdateState(res);
                if (!isMock && res?.phase === 'installing') {
                  return;
                }
              } catch {
                // Fall through to normal startup on auto-install error.
              }
              if (alive) setStartupUpdating(false);
            }
          }
        } catch {
          // Ignore updateState errors during boot.
        }
      }

      if (!alive) return;
      firstScreenShownRef.current = true;

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

    const resumeController = createForegroundResumeController({
      invalidateSessions:
        typeof link.invalidateSessions === 'function' ? () => link.invalidateSessions() : null,
      onResumingChange: (active) => {
        resumingRef.current = active;
      },
      onBackground: () => {
        if (
          typeof link.maybeAutoInstall === 'function' &&
          isSafeForAutoInstall({
            updateState: updateStateRef.current,
            trigger: 'background',
            hasOpenSheet: Boolean(
              confirmRequestRef.current ||
                whatIsNewOpenRef.current ||
                filesSheetRef.current ||
                wakeSheetOpenRef.current,
            ),
            hasCommandDraft: Boolean(commandDraftRef.current.cmd?.trim()),
            isRunningCommand: Boolean(commandDraftRef.current.running),
            activeCalls: activeCallsRef.current + activeTransfersCountRef.current,
            isPairingOpen: Boolean(pairingOpenRef.current),
            isResuming: resumingRef.current,
          })
        ) {
          link.maybeAutoInstall('background').catch(() => {});
        }
      },
      onInvalidate: () => {
        capsStore.invalidate();
      },
      onRefresh: () => {
        refreshFleet(nodeInfo, { background: true });
      },
      onCheckUpdates: () => {
        if (
          shouldCheckOnResume(updateStateRef.current?.last_checked_at, Date.now(), {
            phase: updateStateRef.current?.phase,
          })
        ) {
          checkUpdatesQuietly();
        }
      },
    });

    const onVisibilityChange = () => {
      resumeController.handleVisibilityChange().catch(() => {});
    };

    const unsubNativeResume =
      typeof link.onAppResume === 'function'
        ? link.onAppResume(() => {
            resumeController.handleNativeResume().catch(() => {});
          })
        : () => {};

    const onFocus = () => {
      if (
        document.visibilityState === 'visible' &&
        !resumeController.isHidden() &&
        !resumeController.isResuming() &&
        settings.refreshSeconds > 0
      ) {
        refreshFleet(nodeInfo, { background: true });
      }
    };

    const timer =
      settings.refreshSeconds > 0
        ? window.setInterval(() => {
            if (document.visibilityState === 'visible' && !resumeController.isResuming()) {
              refreshFleet(nodeInfo, { background: true });
            }
          }, settings.refreshSeconds * 1000)
        : null;

    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => {
      if (timer !== null) window.clearInterval(timer);
      resumingRef.current = false;
      unsubNativeResume();
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onVisibilityChange);
    };
  }, [link, capsStore, linkState?.ready, nodeInfo, refreshFleet, settings.refreshSeconds, checkUpdatesQuietly]);

  const registerPopStateInterceptor = useCallback((fn) => {
    popStateInterceptorRef.current = typeof fn === 'function' ? fn : null;
    return () => {
      if (popStateInterceptorRef.current === fn) {
        popStateInterceptorRef.current = null;
      }
    };
  }, []);

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
    if (wakeUiState?.wakingMachines) {
      for (const name of wakeUiState.wakingMachines) out.add(name);
    }
    return out;
  }, [wakeSentAt, wakeUiState]);

  const handleUpdateMachineSys = useCallback((machineName, sys) => {
    if (!machineName || !sys) return;
    setSnapshot((prev) => {
      if (!prev || !Array.isArray(prev.machines)) return prev;
      let changed = false;
      const nextMachines = prev.machines.map((m) => {
        if (m.name !== machineName) return m;
        changed = true;
        return {
          ...m,
          reachable: true,
          status: 'answering',
          asking: false,
          sys,
        };
      });
      return changed ? { ...prev, machines: nextMachines } : prev;
    });
  }, []);

  // Shared "Wake it and continue" controller (one wake and one sheet per machine).
  const wakeController = useMemo(() => {
    const customClock =
      typeof window !== 'undefined' && window.__LEGION_TEST_WAKE_CLOCK__
        ? window.__LEGION_TEST_WAKE_CLOCK__
        : undefined;
    const ctrl = createWakeController({
      link,
      clock: customClock,
      getMachines: () => snapshotRef.current?.machines || [],
      getLocalNode: () => nodeInfoRef.current?.node || null,
      getAllowedSet: () => allowedSetRef.current,
      onWakeSent(machineName) {
        markWakeSent(machineName);
      },
      onWakeTimeout(machineName) {
        setWakeSentAt((prev) => {
          if (!prev[machineName]) return prev;
          const next = { ...prev };
          delete next[machineName];
          return next;
        });
      },
      async onMachineAwake(machineName) {
        setSnapshot((prev) => {
          if (!prev || !Array.isArray(prev.machines)) return prev;
          return {
            ...prev,
            machines: prev.machines.map((m) =>
              m.name === machineName
                ? { ...m, reachable: true, status: 'answering', asking: false, error: null, reason: null }
                : m,
            ),
          };
        });
        await refreshFleet(nodeInfoRef.current, { forceOffline: true });
      },
      onBackgroundComplete({ machine, kind }) {
        const noun = kind === 'transfer' ? 'Transfer' : kind === 'command' ? 'Command' : 'Action';
        showToast(`${noun} started on ${machine}.`, {
          actionLabel: 'Activity',
          actionHref: '#/activity',
        });
      },
    });
    return ctrl;
  }, [link, markWakeSent, refreshFleet, showToast]);

  wakeControllerRef.current = wakeController;

  useEffect(() => {
    setDefaultWakeController(wakeController);
    setWakeUiState(wakeController.getState());
    const unsub = wakeController.subscribe((st) => setWakeUiState(st));
    return () => {
      unsub();
      wakeController.dispose();
      setDefaultWakeController(null);
    };
  }, [wakeController]);

  const ensureAwakeMachine = useCallback(
    (machine, options) => wakeController.ensureAwake(machine, options),
    [wakeController],
  );

  const filesActions = useMemo(
    () => ({
      pushHref(href) {
        const pushDirect = (targetHref) => {
          const cur = routeRef.current;
          if (cur?.section === 'files' && cur.view === 'folder' && !cur.viewer && mainRef.current) {
            scrollByFolderRef.current.set(`${cur.machine || ''}\0${cur.path || '~'}`, mainRef.current.scrollTop);
          }
          if (targetHref && targetHref !== window.location.hash) {
            const parsedTarget = parseRoute(targetHref);
            const sameUnderlyingRoute =
              Boolean(parsedTarget.viewer) &&
              !cur?.viewer &&
              parsedTarget.section === cur?.section &&
              parsedTarget.view === cur?.view &&
              parsedTarget.machine === cur?.machine &&
              parsedTarget.path === cur?.path;
            filesHistoryDepthRef.current += 1;
            viewerHistoryPushedRef.current = sameUnderlyingRoute;
            window.history.pushState(sameUnderlyingRoute ? { legionViewerOverlay: true } : null, '', targetHref);
            window.dispatchEvent(new HashChangeEvent('hashchange'));
          }
        };
        if (href) {
          const parsed = parseRoute(href);
          if (parsed.section === 'files' && parsed.machine) {
            const targetMachine = (snapshotRef.current?.machines || []).find(
              (m) => m.name === parsed.machine,
            );
            const canWake = allowedSetRef.current ? allowedSetRef.current.has('power.wake') : false;
            if (targetMachine && !targetMachine.reachable && targetMachine.canWake && canWake) {
              wakeControllerRef.current?.ensureAwake(targetMachine, {
                kind: 'files',
                label: 'open its files',
                onContinue: () => pushDirect(href),
              });
              return;
            }
          }
        }
        pushDirect(href);
      },
      closeViewer() {
        const cur = routeRef.current;
        const folderHref = routeHref({ ...cur, viewer: null });
        const hadSheet = sheetHistoryRef.current;
        if (hadSheet) {
          sheetHistoryRef.current = false;
          setConfirmRequest(null);
        }
        if (viewerHistoryPushedRef.current) {
          viewerHistoryPushedRef.current = false;
          if (hadSheet) {
            window.history.go(-2);
          } else {
            window.history.back();
          }
          return;
        }
        if (hadSheet) {
          // Deep-linked or cross-route viewer behind a confirm sheet: pop the sheet entry and
          // replace the viewer entry in place so no history entry points at the deleted file.
          pendingReplaceAfterPopRef.current = folderHref;
          window.history.back();
          return;
        }
        window.history.replaceState(null, '', folderHref);
        window.dispatchEvent(new HashChangeEvent('hashchange'));
      },
      openSheet(nextSheet) {
        if (!filesSheetHistoryRef.current) {
          window.history.pushState({ legionFilesSheet: true }, '');
          filesSheetHistoryRef.current = true;
        }
        setFilesSheet(nextSheet);
      },
      closeSheet() {
        setFilesSheet(null);
        if (filesSheetHistoryRef.current) {
          filesSheetHistoryRef.current = false;
          window.history.back();
        }
      },
      startSelect(initialPath) {
        if (filesSheetHistoryRef.current) {
          setFilesSheet(null);
          filesSheetHistoryRef.current = false;
          if (!filesSelectHistoryRef.current) {
            window.history.replaceState({ legionFilesSelect: true }, '');
            filesSelectHistoryRef.current = true;
          } else {
            window.history.back();
          }
        } else if (!filesSelectHistoryRef.current) {
          window.history.pushState({ legionFilesSelect: true }, '');
          filesSelectHistoryRef.current = true;
        }
        setFilesSelecting(true);
        setFilesSelectedPaths(initialPath ? new Set([initialPath]) : new Set());
      },
      endSelect() {
        setFilesSelecting(false);
        setFilesSelectedPaths(new Set());
        if (filesSelectHistoryRef.current) {
          filesSelectHistoryRef.current = false;
          window.history.back();
        }
      },
      finishBulkAction() {
        setFilesSheet(null);
        setFilesSelecting(false);
        setFilesSelectedPaths(new Set());
        let pops = 0;
        if (filesSheetHistoryRef.current) {
          filesSheetHistoryRef.current = false;
          pops += 1;
        }
        if (filesSelectHistoryRef.current) {
          filesSelectHistoryRef.current = false;
          pops += 1;
        }
        if (pops > 0) {
          window.history.go(-pops);
        }
      },
      toggleSelect(path) {
        setFilesSelectedPaths((prev) => toggleSelection(prev, path));
      },
      toggleSelectAll(entries) {
        setFilesSelectedPaths((prev) => {
          if (prev.size >= (entries || []).length && prev.size > 0) return new Set();
          return selectAllEntries(entries);
        });
      },
      pruneSelection(validPaths) {
        setFilesSelectedPaths((prev) => {
          if (!prev || prev.size === 0) return prev;
          let changed = false;
          const next = new Set();
          for (const p of prev) {
            if (validPaths.has(p)) next.add(p);
            else changed = true;
          }
          return changed ? next : prev;
        });
      },
      setSortPref(pref) {
        writeSortPreference(safeLocalStorage(), pref);
        setFilesSortPref(pref);
      },
      setKindFilter(kind) {
        setFilesKindFilter(kind ? String(kind) : '');
      },
      setShowHidden(val) {
        setFilesShowHidden(Boolean(val));
      },
      setViewModeOverride(mode) {
        setFilesViewModeOverride(mode);
      },
      setResolvedViewMode(mode) {
        setFilesResolvedViewMode(mode);
      },
      setMeta(meta) {
        setFilesMeta((prev) => ({ ...prev, ...meta }));
      },
      setSearchSummary(summary) {
        setFilesSearchSummary(summary);
      },
      registerHandlers(handlers) {
        filesBulkHandlersRef.current = handlers || {};
      },
      bulkSend(machine, items) {
        filesBulkHandlersRef.current.onBulkSend?.(machine, items);
      },
      bulkDownload(machine, items) {
        filesBulkHandlersRef.current.onBulkDownload?.(machine, items);
      },
      bulkStar(machine, items) {
        filesBulkHandlersRef.current.onBulkStar?.(machine, items);
      },
      bulkDelete(machine, items) {
        filesBulkHandlersRef.current.onBulkDelete?.(machine, items);
      },
    }),
    [],
  );

  const handleOpenDeviceTransfer = useCallback(
    async (transfer) => {
      if (!transfer) return;
      if (typeof link.openTransfer === 'function') {
        try {
          const res = await link.openTransfer(transfer.id);
          if (res?.viewer?.machine && res?.viewer?.path) {
            const dir = parentFilePath(res.viewer.path) || '~';
            filesActions.pushHref(
              routeHref({
                section: 'files',
                view: 'folder',
                machine: res.viewer.machine,
                path: dir,
                viewer: { machine: res.viewer.machine, path: res.viewer.path },
              }),
            );
          }
          return;
        } catch (err) {
          showToast(
            plainErrorMessage(err, 'Could not open this download.', {
              technical: Boolean(settingsRef.current?.technical),
              deviceNoun,
            }),
          );
          return;
        }
      }
      if (transfer.machine && transfer.path) {
        const dir = parentFilePath(transfer.path) || '~';
        filesActions.pushHref(
          routeHref({
            section: 'files',
            view: 'folder',
            machine: transfer.machine,
            path: dir,
            viewer: { machine: transfer.machine, path: transfer.path },
          }),
        );
      }
    },
    [link, filesActions, showToast],
  );

  const machines = snapshot?.machines || [];
  const selectedMachine = route.machine
    ? machines.find((m) => m.name === route.machine) || null
    : null;

  const isFilesRoot = route.section === 'files' && route.view === 'home' && !route.viewer;
  const isFilesFolder = route.section === 'files' && route.view === 'folder';
  const isFilesSearch = route.section === 'files' && route.view === 'search';

  const isRootPlace =
    (route.section === 'machines' && !route.machine) ||
    isFilesRoot ||
    (route.section === 'activity' && !route.jobId) ||
    (route.section === 'actions' && !route.action && !route.allTools);

  const inMachinePhone = !wide && route.section === 'machines' && Boolean(route.machine);
  const isPhoneCommand =
    inMachinePhone &&
    route.tab === 'command' &&
    Boolean(selectedMachine?.reachable || (selectedMachine?.canWake && allowedSet?.has('power.wake')));
  const isPhoneMachineFiles =
    inMachinePhone && route.tab === 'files' && Boolean(selectedMachine?.reachable);
  const isActionsRoot =
    route.section === 'actions' && !route.action && !route.allTools;
  const isWideSplitHost =
    wide &&
    (route.section === 'machines' ||
      route.section === 'files' ||
      route.section === 'activity' ||
      route.section === 'settings' ||
      (route.section === 'actions' && Boolean(route.allTools)));
  const isWideSplitScreen = isWideSplitHost && !filesSelecting;
  const hasFilesDock =
    (route.section === 'files' &&
      (filesSelecting || (!wide && (isFilesRoot || isFilesFolder || isFilesSearch)))) ||
    isPhoneMachineFiles;
  const hasDock = isActionsRoot || isPhoneCommand || hasFilesDock;

  // Hide bottom bar on phone for Settings, single-item detail pages, and Files selection mode.
  const showBottomNav =
    wide ||
    isRootPlace ||
    inMachinePhone ||
    (route.section === 'files' && !filesSelecting);

  function goBack() {
    if (route.section === 'files') {
      if (filesSheet) {
        filesActions.closeSheet();
        return;
      }
      if (filesSelecting) {
        filesActions.endSelect();
        return;
      }
      if (filesHistoryDepthRef.current > 0) {
        window.history.back();
        return;
      }
      const step = filesFallbackBackRoute(route, { hasSheet: Boolean(filesSheet), selecting: filesSelecting });
      if (step?.action === 'navigate' && step.href) {
        window.location.hash = step.href;
        return;
      }
      if (window.history.length > 1) {
        window.history.back();
      } else {
        window.location.hash = '#/machines';
      }
      return;
    }
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

  // Horizontal navigation gestures: root-place swipe, machine-part swipe, non-Android edge back.
  function finishHorizontalSwipe(start, endX, endY) {
    if (wide || !start) return;

    const dx = endX - start.x;
    const dy = endY - start.y;
    const elapsedMs = Date.now() - start.t;

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

    if (start.edgeBackOnly) return;

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

  function onMainTouchStart(e) {
    if (!e.touches || e.touches.length !== 1) {
      touchSwipeRef.current = null;
      return;
    }
    const t = e.touches[0];
    const allowEdgeBack =
      !wide && !isRootPlace && !hasNativeEdgeBack() && t.clientX <= EDGE_BACK_ZONE_PX;
    const ignored = shouldIgnoreSwipeTarget(e.target, mainRef.current);
    if (ignored && !allowEdgeBack) {
      touchSwipeRef.current = null;
      return;
    }
    // Own real touch swipes via touch events so browser pointercancel on links never drops edge-back.
    swipeRef.current = null;
    touchSwipeRef.current = {
      id: t.identifier,
      x: t.clientX,
      y: t.clientY,
      t: Date.now(),
      edgeBackOnly: ignored && allowEdgeBack,
    };
  }

  function onMainTouchEnd(e) {
    const start = touchSwipeRef.current;
    touchSwipeRef.current = null;
    if (!start || !e.changedTouches) return;
    let touch = null;
    for (let i = 0; i < e.changedTouches.length; i += 1) {
      if (e.changedTouches[i].identifier === start.id) {
        touch = e.changedTouches[i];
        break;
      }
    }
    if (!touch) return;
    finishHorizontalSwipe(start, touch.clientX, touch.clientY);
  }

  function onMainTouchCancel() {
    touchSwipeRef.current = null;
  }

  function onMainPointerDown(e) {
    if (e.button > 0) return;
    const allowEdgeBack =
      !wide && !isRootPlace && !hasNativeEdgeBack() && e.clientX <= EDGE_BACK_ZONE_PX;
    const ignored = shouldIgnoreSwipeTarget(e.target, mainRef.current);
    if (ignored && !allowEdgeBack) return;
    swipeRef.current = {
      id: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      t: Date.now(),
      edgeBackOnly: ignored && allowEdgeBack,
    };
  }

  function onMainPointerUp(e) {
    const start = swipeRef.current;
    swipeRef.current = null;
    if (!start || start.id !== e.pointerId) return;
    finishHorizontalSwipe(start, e.clientX, e.clientY);
  }

  function onMainPointerCancel() {
    swipeRef.current = null;
  }

  const handlePullRefresh = useCallback(async () => {
    if (typeof screenRefreshRef.current === 'function') {
      await screenRefreshRef.current();
      return;
    }
    const cur = routeRef.current;
    if (cur?.section === 'settings') {
      if (cur.page === 'about' && typeof link.checkUpdates === 'function') {
        await checkUpdatesQuietly();
      }
      if (typeof link.state === 'function') {
        const st = await link.state().catch(() => null);
        if (st) mergeBackendState(st);
      }
      await refreshFleet(nodeInfoRef.current, { background: true });
      return;
    }
    if (cur?.section === 'actions') {
      const primary = nodeInfoRef.current?.node;
      if (primary) {
        capsStore.invalidate(primary);
        const entry = await capsStore.ensure(primary, { force: true }).catch(() => null);
        if (Array.isArray(entry?.tools) && entry.tools.length > 0 && nodeInfoRef.current?.node === primary) {
          setTools(entry.tools);
          sessionCache.set(`tools:${primary}`, entry.tools);
        }
      }
      await refreshFleet(nodeInfoRef.current, { forceOffline: true });
      return;
    }
    await refreshFleet(nodeInfoRef.current, { forceOffline: true });
    triggerLearnInBackground();
  }, [link, capsStore, sessionCache, checkUpdatesQuietly, mergeBackendState, refreshFleet, triggerLearnInBackground]);

  const pullDisabled = Boolean(
    confirmRequest ||
      whatIsNewOpen ||
      pairingOpen ||
      filesSheet ||
      route.viewer ||
      filesSelecting ||
      wakeUiState?.sheet,
  );

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
    if (isWideSplitScreen) {
      return null;
    }

    // Files headers: selection mode, folder, search, sync, favourites, recent, and root.
    if (route.section === 'files') {
      if (filesSelecting) {
        const sum = selectionSummary(filesMeta.entries, filesSelectedPaths);
        const allPicked = filesMeta.entries.length > 0 && sum.count >= filesMeta.entries.length;
        return html`
          <header class="top sub">
            <button
              type="button"
              class="btn icon back"
              aria-label="Stop selecting"
              onClick=${() => filesActions.endSelect()}
            >
              <${Icon} name="back" />
            </button>
            <h1>
              ${`${sum.count} selected`}
              ${sum.allSizesKnown ? html`<small>${formatBytes(sum.totalBytes)}</small>` : null}
            </h1>
            <button
              type="button"
              class="btn small quiet"
              onClick=${() => filesActions.toggleSelectAll(filesMeta.entries)}
            >
              ${allPicked ? 'Deselect all' : 'Select all'}
            </button>
          </header>
        `;
      }
      if (!wide && route.view === 'folder') {
        const title = folderDisplayName(filesMeta.resolvedPath || route.path, {
          technical: Boolean(settings.technical),
        });
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>${title}</h1>
          </header>
        `;
      }
      if (!wide && route.view === 'search') {
        const sub = filesSearchSummary.searching
          ? 'Searching…'
          : route.machine
            ? `${filesSearchSummary.count} found in ${folderDisplayName(route.path)}`
            : filesSearchSummary.machineCount > 0
              ? `${filesSearchSummary.count} found on ${filesSearchSummary.machineCount} ${filesSearchSummary.machineCount === 1 ? 'machine' : 'machines'}`
              : 'Search across your machines';
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>Search<small>${sub}</small></h1>
          </header>
        `;
      }
      if (!wide && route.view === 'sync') {
        const sf = (filesMeta.syncFolders || []).find((f) => f.id === route.syncId);
        const memberCount = sf?.members?.length || 0;
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>
              ${sf?.name || 'Synced folder'}
              ${memberCount > 0 ? html`<small>Kept in sync on ${memberCount} machines</small>` : null}
            </h1>
          </header>
        `;
      }
      if (!wide && route.view === 'favourites') {
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>Favourites</h1>
          </header>
        `;
      }
      if (!wide && route.view === 'recent') {
        return html`
          <header class="top sub">
            <button type="button" class="btn icon back" aria-label="Back" onClick=${goBack}>
              <${Icon} name="back" />
            </button>
            <h1>Recent<small>Opened on this ${deviceNoun}</small></h1>
          </header>
        `;
      }

      const onlineCount = machines.filter((m) => m.reachable).length;
      const filesSub =
        onlineCount === 0
          ? 'No machines are on'
          : onlineCount === 1
            ? '1 machine is on'
            : `${onlineCount} machines are on`;
      return html`
        <header class="top">
          <h1>
            Files
            ${machines.length > 0 ? html`<small>${filesSub}</small>` : null}
          </h1>
          ${!wide
            ? html`
                <a class="btn icon" href="#/settings" aria-label="Settings">
                  <${Icon} name="gear" />
                </a>
              `
            : null}
        </header>
      `;
    }

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
      const activeDeviceCount = deviceTransfers.filter(
        (t) => t.state === 'running' || t.state === 'queued',
      ).length;
      const runningCount = (snapshot?.runningJobs || []).length + activeDeviceCount;
      return html`
        <header class="top">
          <h1>
            Activity
            ${runningCount > 0
              ? html`<small>${runningCount} ${runningCount === 1 ? 'thing' : 'things'} running</small>`
              : null}
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
    if (hasFilesDock) {
      const dockRoute = isPhoneMachineFiles
        ? {
            section: 'files',
            view: 'folder',
            machine: route.machine,
            path: filesMeta.resolvedPath || '~',
          }
        : route;
      return html`
        <${FilesDock}
          route=${dockRoute}
          filesState=${{
            caps: filesMeta.caps,
            selecting: isPhoneMachineFiles ? false : filesSelecting,
            selectedPaths: filesSelectedPaths,
            entries: filesMeta.entries,
            resolvedPath: filesMeta.resolvedPath,
            sortPref: filesSortPref,
            kindFilter: filesKindFilter,
            resolvedViewMode: filesResolvedViewMode,
          }}
          filesActions=${filesActions}
          onNavigateHref=${filesActions.pushHref}
        />
      `;
    }
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
            onEnsureAwake=${ensureAwakeMachine}
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

    // Root 4 places (plus Settings at the foot of the wide rail).
    return html`
      <nav class="nav" aria-label="Main">
        <a
          href="#/machines"
          aria-current=${route.section === 'machines' ? 'page' : undefined}
        >
          <${Icon} name="machines" />Machines
        </a>
        <a
          href="#/files"
          aria-current=${route.section === 'files' ? 'page' : undefined}
        >
          <${Icon} name="files" />Files
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

  const hideToastOnRoute =
    route.section === 'activity' &&
    Boolean(toast?.suppressOnActivity || toast?.actionHref === '#/activity');
  const visibleToast = hideToastOnRoute ? null : toast;

  const appClasses = [
    'app',
    !showBottomNav ? 'standalone' : '',
    hasDock ? 'docked' : '',
    isWideSplitScreen ? 'split-screen' : '',
  ]
    .filter(Boolean)
    .join(' ');

  return html`
    <div ref=${shellRef} class="shell">
      <${IconSprite} />
      ${startupUpdating
        ? html`<div class="startup-update" role="status" aria-live="polite">Updating…</div>`
        : booting
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
                pairingController=${pairingController}
                pairingState=${pairingState}
                onCancel=${linkState?.ready ? () => setPairingOpen(false) : null}
                onConnected=${(next) => {
                  setPairingOpen(false);
                  setFleetError(null);
                  capsStore.invalidate();
                  setPairNotice(
                    next?.last_paired ? { summary: next.last_paired, screen: 'machines' } : null,
                  );
                  hydrate(next);
                }}
              />
            `
          : html`
              <div class=${appClasses}>
                ${renderHeader()}
                ${visibleToast
                  ? html`
                      <div
                        class=${visibleToast.tone === 'error' ? 'toast-banner error' : 'toast-banner'}
                        role=${visibleToast.tone === 'error' ? 'alert' : 'status'}
                      >
                        <span>${visibleToast.message}</span>
                        ${visibleToast.actionLabel && visibleToast.onAction
                          ? html`
                              <button
                                type="button"
                                class="btn small primary"
                                onClick=${() => {
                                  const fn = visibleToast.onAction;
                                  clearToast();
                                  fn();
                                }}
                              >
                                ${visibleToast.actionLabel}
                              </button>
                            `
                          : visibleToast.actionLabel && visibleToast.actionHref
                            ? html`
                                <a
                                  class="btn small primary"
                                  href=${visibleToast.actionHref}
                                  onClick=${clearToast}
                                >
                                  ${visibleToast.actionLabel}
                                </a>
                              `
                            : null}
                        <button
                          type="button"
                          class="btn small quiet"
                          aria-label="Dismiss"
                          onClick=${clearToast}
                        >
                          OK
                        </button>
                      </div>
                    `
                  : null}
                <${ScrollContainer}
                  as="main"
                  containerRef=${mainRef}
                  className=${isPhoneCommand ? 'main command-stream' : isWideSplitHost ? 'main split-host' : 'main'}
                  onRefresh=${isWideSplitHost ? null : handlePullRefresh}
                  disabled=${pullDisabled || isWideSplitHost}
                  onTouchStart=${onMainTouchStart}
                  onTouchEnd=${onMainTouchEnd}
                  onTouchCancel=${onMainTouchCancel}
                  onPointerDown=${onMainPointerDown}
                  onPointerUp=${onMainPointerUp}
                  onPointerCancel=${onMainPointerCancel}
                >
                  ${route.section === 'machines'
                    ? html`
                        <${MachinesView}
                          snapshot=${snapshot}
                          error=${fleetError}
                          pairNotice=${pairNotice?.screen === 'machines' ? pairNotice.summary : null}
                          onDismissPairNotice=${() => setPairNotice(null)}
                          updateState=${updateState}
                          onOpenWhatIsNew=${() => setWhatIsNewOpen(true)}
                          onDismissUpdateNotice=${handleDismissUpdateNotice}
                          loading=${refreshing}
                          wide=${wide}
                          route=${route}
                          link=${link}
                          capsStore=${capsStore}
                          folderChanges=${folderChanges}
                          onObserveTransferJobs=${observeTransferJobs}
                          onRegisterRefresh=${registerScreenRefresh}
                          nodeInfo=${nodeInfo}
                          tools=${tools}
                          allowedSet=${allowedSet}
                          settings=${settings}
                          deviceNoun=${deviceNoun}
                          thumbCache=${thumbCache}
                          filesState=${{
                            sortPref: filesSortPref,
                            kindFilter: filesKindFilter,
                            showHidden: filesShowHidden,
                            selecting: filesSelecting,
                            selectedPaths: filesSelectedPaths,
                            sheet: filesSheet,
                            viewModeOverride: filesViewModeOverride,
                            resolvedViewMode: filesResolvedViewMode,
                          }}
                          filesActions=${filesActions}
                          wakePendingSet=${wakePendingSet}
                          onEnsureAwake=${ensureAwakeMachine}
                          onWakeSent=${markWakeSent}
                          onRequestConfirm=${openConfirm}
                          onRefresh=${() => {
                            const p = refreshFleet(nodeInfo, { forceOffline: true });
                            triggerLearnInBackground();
                            return p;
                          }}
                          onUpdateMachineSys=${handleUpdateMachineSys}
                          commandRuns=${{ byMachine: commandRunsByMachine, draft: commandDraft }}
                          onCommandRunsChange=${commandRunsApi}
                        />
                      `
                    : route.section === 'files'
                      ? html`
                          <${FilesView}
                            route=${route}
                            wide=${wide}
                            snapshot=${snapshot}
                            link=${link}
                            capsStore=${capsStore}
                            folderChanges=${folderChanges}
                            onRegisterRefresh=${registerScreenRefresh}
                            onRegisterPopState=${registerPopStateInterceptor}
                            nodeInfo=${nodeInfo}
                            tools=${tools}
                            allowedSet=${allowedSet}
                            settings=${settings}
                            deviceNoun=${deviceNoun}
                            thumbCache=${thumbCache}
                            filesState=${{
                              sortPref: filesSortPref,
                              kindFilter: filesKindFilter,
                              showHidden: filesShowHidden,
                              selecting: filesSelecting,
                              selectedPaths: filesSelectedPaths,
                              sheet: filesSheet,
                              viewModeOverride: filesViewModeOverride,
                              resolvedViewMode: filesResolvedViewMode,
                            }}
                            filesActions=${filesActions}
                            wakePendingSet=${wakePendingSet}
                            onRefreshFleet=${(opts) => refreshFleet(nodeInfo, opts)}
                            onEnsureAwake=${ensureAwakeMachine}
                            onRequestConfirm=${openConfirm}
                            onToast=${showToast}
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
                              deviceNoun=${deviceNoun}
                              deviceTransfers=${deviceTransfers}
                              onObserveTransferJobs=${observeTransferJobs}
                              onRegisterRefresh=${registerScreenRefresh}
                              onOpenTransfer=${handleOpenDeviceTransfer}
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
                                folderChanges=${folderChanges}
                                onRegisterRefresh=${registerScreenRefresh}
                                settings=${settings}
                                wide=${wide}
                                query=${actionsQuery}
                                onWakeSent=${markWakeSent}
                                onEnsureAwake=${ensureAwakeMachine}
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
                                capsStore=${capsStore}
                                onRegisterRefresh=${registerScreenRefresh}
                                linkState=${linkState}
                                pairingController=${pairingController}
                                pairingState=${pairingState}
                                nodeInfo=${nodeInfo}
                                tools=${tools}
                                allowedSet=${allowedSet}
                                snapshot=${snapshot}
                                settings=${settings}
                                pairNotice=${pairNotice?.screen === 'connections' ? pairNotice.summary : null}
                                onDismissPairNotice=${() => setPairNotice(null)}
                                updateState=${updateState}
                                onCheckUpdates=${handleCheckUpdates}
                                onInstallUpdate=${handleInstallUpdate}
                                onSetAutoUpdate=${handleSetAutoUpdate}
                                onDismissUpdateNotice=${handleDismissUpdateNotice}
                                onUpdateSettings=${updateSettings}
                                onRequestConfirm=${openConfirm}
                                onAcceptIdentityChange=${handleAcceptIdentityChange}
                                onDismissIdentityChange=${handleDismissIdentityChange}
                                onConnected=${(next) => {
                                  setFleetError(null);
                                  capsStore.invalidate();
                                  folderChanges.clearCache();
                                  setPairNotice(
                                    next?.last_paired
                                      ? { summary: next.last_paired, screen: 'connections' }
                                      : null,
                                  );
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
                                        capsStore.invalidate();
                                        folderChanges.clearCache();
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
                                  capsStore.invalidate();
                                  folderChanges.clearCache();
                                  setSnapshot(null);
                                  setTools([]);
                                  const st = await link.state();
                                  await hydrate(st);
                                }}
                              />
                            `}
                <//>
                ${renderDock()}
                ${renderNav()}
              </div>
            `}
      ${whatIsNewOpen && updateState?.just_updated
        ? html`
            <${Sheet}
              title=${`What is new in ${updateState.just_updated.version}`}
              onClose=${() => setWhatIsNewOpen(false)}
            >
              <p>${updateState.just_updated.notes}</p>
              ${settings.technical
                ? html`
                    <div class="list spaced">
                      <dl class="kv">
                        <dt>channel</dt>
                        <dd>${updateState.just_updated.channel}</dd>
                        <dt>build</dt>
                        <dd>${updateState.just_updated.build}</dd>
                        <dt>sequence</dt>
                        <dd>${updateState.just_updated.sequence}</dd>
                      </dl>
                    </div>
                  `
                : null}
              <div class="actions">
                <button type="button" class="btn primary full" onClick=${handleDismissUpdateNotice}>
                  Done
                </button>
              </div>
            <//>
          `
        : null}
      ${confirmRequest
        ? html`
            <${ConfirmSheet}
              request=${confirmRequest}
              technical=${Boolean(settings.technical)}
              onClose=${closeConfirm}
            />
          `
        : null}
      ${wakeUiState?.sheet
        ? html`
            <${WakeSheet}
              sheet=${wakeUiState.sheet}
              onConfirm=${() => wakeController.confirmWake(wakeUiState.sheet.machineName)}
              onRetry=${() => wakeController.retry(wakeUiState.sheet.machineName)}
              onDismiss=${() => wakeController.dismiss(wakeUiState.sheet.machineName)}
            />
          `
        : null}
    </div>
  `;
}

render(html`<${App} />`, document.getElementById('root'));
