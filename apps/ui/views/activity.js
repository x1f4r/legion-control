// Activity screen and job detail: plain by default, including machine transfer jobs and device
// download/upload queue rows with progress, pause/resume/cancel, and tap-to-open finished downloads.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Notice, ProgressBar, SwipeRow } from './common.js';
import { SplitView } from './split-view.js';
import {
  formatActivityDuration,
  formatActivitySubtitle,
  formatDeviceTransferSubtitle,
  formatDeviceTransferTitle,
  formatFriendlyRelativeTime,
  formatTransferJobSubtitle,
  friendlyDeviceTransferState,
  friendlyJobName,
  friendlyJobState,
  groupActivity,
  jobDurationSeconds,
  transferProgressPct,
} from '../lib/activity.js';
import { callWithTimeout, DETAIL_TIMEOUT_MS } from '../lib/fleet.js';
import { formatBytes } from '../lib/format.js';
import { routeHref } from '../lib/route.js';

function targetFor(machineName, nodeName) {
  return machineName || nodeName || undefined;
}

export function ActivityDetail({
  machineName,
  nodeName,
  jobId,
  link,
  allowedSet,
  technical = false,
  onObserveTransferJobs = null,
  onRegisterRefresh = null,
  onChanged,
}) {
  const [status, setStatus] = useState(null);
  const [logText, setLogText] = useState('');
  const [error, setError] = useState(null);
  const [stopping, setStopping] = useState(false);
  const offsetRef = useRef(0);
  const pollRef = useRef(null);
  const on = targetFor(machineName, nodeName);
  const canCancel = allowedSet ? allowedSet.has('job.cancel') : true;

  useEffect(() => {
    let alive = true;
    let timer = null;
    offsetRef.current = 0;
    setLogText('');
    setStatus(null);
    setError(null);

    async function poll() {
      if (!alive) return;
      try {
        const [st, logs] = await Promise.all([
          link.call('job.status', { id: jobId }, { on }),
          link.call('job.logs', { id: jobId, offset: offsetRef.current }, { on }),
        ]);
        if (!alive) return;
        setStatus(st);
        if (onObserveTransferJobs && st) {
          onObserveTransferJobs([{ ...st, machine: machineName }]);
        }
        if (logs?.content) {
          setLogText((prev) => prev + logs.content);
        }
        if (typeof logs?.next_offset === 'number') {
          offsetRef.current = logs.next_offset;
        }
        if (st.state === 'starting' || st.state === 'running') {
          timer = window.setTimeout(poll, 700);
        }
      } catch (err) {
        if (alive) setError(err);
      }
    }

    pollRef.current = poll;
    poll();
    return () => {
      alive = false;
      pollRef.current = null;
      if (timer) window.clearTimeout(timer);
    };
  }, [machineName, jobId, onObserveTransferJobs]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(() => (pollRef.current ? pollRef.current() : Promise.resolve()));
  }, [onRegisterRefresh, machineName, jobId]);

  async function onStop() {
    setStopping(true);
    setError(null);
    try {
      await link.call('job.cancel', { id: jobId }, { on });
      const st = await link.call('job.status', { id: jobId }, { on });
      setStatus(st);
      if (onChanged) onChanged();
    } catch (err) {
      setError(err);
    } finally {
      setStopping(false);
    }
  }

  const normalizedJob = status
    ? { ...status, command: status.cmd || (Array.isArray(status.argv) ? status.argv.join(' ') : '') }
    : null;
  const active = status && (status.state === 'starting' || status.state === 'running');
  const title = normalizedJob ? friendlyJobName(normalizedJob) : 'Activity';
  const stateInfo = normalizedJob ? friendlyJobState(normalizedJob, technical) : null;
  const dur = normalizedJob ? jobDurationSeconds(normalizedJob) : null;
  const pct = status?.kind === 'transfer' ? transferProgressPct(status.progress) : null;

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}

      ${status
        ? html`
            <div class="list">
              <dl class=${technical ? 'kv' : 'kv plain'}>
                <dt>${technical ? 'name' : 'What'}</dt>
                <dd>${title}</dd>
                <dt>${technical ? 'machine' : 'Machine'}</dt>
                <dd>${machineName}</dd>
                <dt>${technical ? 'state' : 'Status'}</dt>
                <dd>
                  <span class=${`state ${stateInfo.cls}`}>
                    ${active && pct != null ? `${pct}%` : stateInfo.label}
                  </span>
                </dd>
                ${status.progress?.bytes_total
                  ? html`
                      <dt>${technical ? 'bytes' : 'Progress'}</dt>
                      <dd>
                        ${technical
                          ? `${status.progress.bytes_done || 0} / ${status.progress.bytes_total} bytes`
                          : `${formatBytes(status.progress.bytes_done || 0)} of ${formatBytes(status.progress.bytes_total)}`}
                      </dd>
                    `
                  : null}
                ${status.created
                  ? html`
                      <dt>${technical ? 'started' : 'Started'}</dt>
                      <dd>${formatFriendlyRelativeTime(status.created)}</dd>
                    `
                  : null}
                ${!active && dur != null
                  ? html`
                      <dt>${technical ? 'duration' : 'How long'}</dt>
                      <dd>${formatActivityDuration(dur, technical)}</dd>
                    `
                  : null}
                ${technical && status.from
                  ? html`
                      <dt>from</dt>
                      <dd>${status.from}</dd>
                    `
                  : null}
                ${technical && status.to
                  ? html`
                      <dt>to</dt>
                      <dd>${status.to}</dd>
                    `
                  : null}
                ${technical && status.hash
                  ? html`
                      <dt>hash</dt>
                      <dd>${status.hash}</dd>
                    `
                  : null}
                ${technical && status.kind === 'transfer'
                  ? html`
                      <dt>tool</dt>
                      <dd>fs.transfer</dd>
                    `
                  : null}
                ${technical && normalizedJob.command
                  ? html`
                      <dt>command</dt>
                      <dd>${normalizedJob.command}</dd>
                    `
                  : null}
                ${technical
                  ? html`
                      <dt>job id</dt>
                      <dd>${status.id}</dd>
                    `
                  : null}
              </dl>
            </div>

            <details class="more spaced" open=${technical || undefined}>
              <summary>Show what it printed</summary>
              <pre class="out wrap">
${logText || (active ? 'Waiting for output…' : 'Nothing was printed.')}</pre>
            </details>

            ${active && canCancel
              ? html`
                  <div class="actions">
                    <button
                      type="button"
                      class="btn danger full"
                      disabled=${stopping}
                      onClick=${onStop}
                    >
                      ${stopping ? 'Stopping…' : 'Stop'}
                    </button>
                  </div>
                `
              : null}
          `
        : !error
          ? html`<div class="empty">Loading…</div>`
          : null}
    </div>
  `;
}

export function ActivityRow({
  job,
  href,
  selected = false,
  includeMachine = true,
  technical = false,
  canCancel = false,
  onStop,
}) {
  const active = job.state === 'running' || job.state === 'starting';
  const isTransfer = job.kind === 'transfer';
  const pct = isTransfer ? transferProgressPct(job.progress) : null;
  const baseState = friendlyJobState(job, technical);
  const stateInfo =
    isTransfer && active && pct != null
      ? { label: `${pct}%`, cls: 'running' }
      : isTransfer && job.state === 'failed' && !technical
        ? { label: 'Stopped', cls: 'failed' }
        : baseState;

  const title = technical ? job.command || job.name || job.id : friendlyJobName(job);
  const subtitle = isTransfer
    ? formatTransferJobSubtitle(job, { includeMachine, technical })
    : formatActivitySubtitle(job, { includeMachine, technical });
  const wrapSub = isTransfer && job.state === 'failed' && !technical;
  const rowCls = ['row', selected ? 'selected' : ''].filter(Boolean).join(' ');

  const rowEl = html`
    <a class=${rowCls} href=${href} aria-current=${selected ? 'true' : undefined}>
      <span class="body">
        <span class=${technical ? 'title mono' : 'title'}>${title}</span>
        ${subtitle || (active && pct != null)
          ? html`
              <span class=${wrapSub ? 'sub wrap' : 'sub'}>
                ${subtitle}
                ${active && pct != null ? html`<${ProgressBar} value=${pct} />` : null}
              </span>
            `
          : null}
      </span>
      <span class=${`state ${stateInfo.cls}`}>${stateInfo.label}</span>
    </a>
  `;

  if (active && canCancel && onStop) {
    return html`
      <${SwipeRow} actionLabel="Stop" onAction=${() => onStop(job)}>
        ${rowEl}
      <//>
    `;
  }
  return rowEl;
}

export function DeviceTransferRow({
  transfer,
  deviceNoun = 'phone',
  technical = false,
  canControl = false,
  onControl,
  onOpen,
}) {
  const st = transfer.state;
  const isActive = st === 'running' || st === 'queued';
  const isPaused = st === 'paused';
  const isFailed = st === 'failed';
  const isDone = st === 'done';
  const pct = transferProgressPct(transfer);
  const stateInfo = friendlyDeviceTransferState(transfer, technical);
  const title = formatDeviceTransferTitle(transfer, technical);
  const sub = formatDeviceTransferSubtitle(transfer, { deviceNoun, technical });

  if (isDone && transfer.canOpen && onOpen) {
    return html`
      <button type="button" class="row" onClick=${() => onOpen(transfer)}>
        <span class="body">
          <span class=${technical ? 'title mono' : 'title'}>${title}</span>
          <span class="sub">${sub}</span>
        </span>
        <span class=${`state ${stateInfo.cls}`}>${stateInfo.label}</span>
      </button>
    `;
  }

  return html`
    <div class="row">
      <div class="body">
        <span class=${technical ? 'title mono' : 'title'}>${title}</span>
        <span class="sub wrap">
          ${sub}
          ${(isActive || isPaused) && pct != null ? html`<${ProgressBar} value=${pct} />` : null}
        </span>
        ${canControl && (isActive || isPaused || isFailed)
          ? html`
              <div class="transfer-ctl">
                ${isActive
                  ? html`
                      <button
                        type="button"
                        class="btn small"
                        onClick=${() => onControl(transfer.id, 'pause')}
                      >
                        Pause
                      </button>
                    `
                  : null}
                ${isPaused || isFailed
                  ? html`
                      <button
                        type="button"
                        class="btn small primary"
                        onClick=${() => onControl(transfer.id, 'resume')}
                      >
                        Resume
                      </button>
                    `
                  : null}
                <button
                  type="button"
                  class="btn small quiet"
                  onClick=${() => onControl(transfer.id, 'cancel')}
                >
                  Cancel
                </button>
              </div>
            `
          : null}
      </div>
      <span class=${`state ${stateInfo.cls}`}>${stateInfo.label}</span>
    </div>
  `;
}

export function ActivityGroups({
  jobs,
  deviceTransfers = [],
  includeMachine = true,
  technical = false,
  deviceNoun = 'phone',
  canCancel = false,
  canControlTransfers = false,
  selectedKey = '',
  hideFirstLabel = false,
  hrefForJob,
  onStopJob,
  onControlTransfer,
  onOpenTransfer,
  emptyText = 'Nothing has run yet. Things you start here or on a machine appear in this list.',
}) {
  const combined = useMemo(() => {
    if (jobs === null) return null;
    const out = (jobs || []).map((j) => ({ _kind: 'job', ...j }));
    for (const t of deviceTransfers || []) {
      const mappedState =
        t.state === 'running' ||
        t.state === 'queued' ||
        t.state === 'paused' ||
        t.state === 'cancelling'
          ? 'running'
          : t.state === 'done'
            ? 'succeeded'
            : t.state === 'failed'
              ? 'failed'
              : 'cancelled';
      out.push({
        _kind: 'transfer',
        _transfer: t,
        id: t.id,
        state: mappedState,
        created: t.created,
        finished: t.finished,
      });
    }
    return out;
  }, [jobs, deviceTransfers]);

  const groups = useMemo(() => groupActivity(combined || []), [combined]);

  if (combined === null) {
    return html`<div class="empty">Loading…</div>`;
  }
  if (groups.length === 0) {
    return html`<div class="empty">${emptyText}</div>`;
  }

  return html`
    <div>
      ${groups.map(
        (group, idx) => html`
          <div key=${group.key}>
            ${!(hideFirstLabel && idx === 0) ? html`<p class="label">${group.label}</p>` : null}
            <div class="list">
              ${group.items.map((item) => {
                if (item._kind === 'transfer') {
                  return html`
                    <${DeviceTransferRow}
                      key=${`dt:${item.id}`}
                      transfer=${item._transfer}
                      deviceNoun=${deviceNoun}
                      technical=${technical}
                      canControl=${canControlTransfers}
                      onControl=${onControlTransfer}
                      onOpen=${onOpenTransfer}
                    />
                  `;
                }
                const key = `${item.machine || ''}:${item.id}`;
                return html`
                  <${ActivityRow}
                    key=${key}
                    job=${item}
                    href=${hrefForJob(item)}
                    selected=${selectedKey === key}
                    includeMachine=${includeMachine}
                    technical=${technical}
                    canCancel=${canCancel}
                    onStop=${onStopJob}
                  />
                `;
              })}
            </div>
          </div>
        `,
      )}
    </div>
  `;
}

export function ActivityView({
  route,
  snapshot,
  link,
  nodeInfo,
  allowedSet,
  settings,
  wide,
  deviceNoun = 'phone',
  deviceTransfers = [],
  onObserveTransferJobs = null,
  onRegisterRefresh = null,
  onOpenTransfer,
  onRefreshFleet,
}) {
  const [jobsByMachine, setJobsByMachine] = useState(null);
  const [error, setError] = useState(null);
  const abortRef = useRef(null);
  const localNode = nodeInfo?.node || 'this node';
  const technical = Boolean(settings?.technical);
  const canCancel = allowedSet ? allowedSet.has('job.cancel') : true;
  const canControlTransfers = typeof link?.transferControl === 'function';

  const targetNames = useMemo(() => {
    const set = new Set();
    if (localNode) set.add(localNode);
    for (const m of snapshot?.machines || []) {
      if (m && m.reachable) set.add(m.name);
    }
    return Array.from(set).sort();
  }, [snapshot?.machines, localNode]);
  const targetKey = targetNames.join('\0');

  async function load(names = targetNames) {
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    setError(null);

    const activeSet = new Set(names);
    let anySucceeded = false;
    let firstError = null;

    await Promise.all(
      names.map(async (machine) => {
        const on = targetFor(machine, localNode);
        try {
          const res = await callWithTimeout(
            link,
            'job.list',
            { limit: 50 },
            { on, signal: ctrl.signal },
            DETAIL_TIMEOUT_MS,
          );
          if (ctrl.signal.aborted) return;
          anySucceeded = true;
          const entries = (res?.jobs || []).map((job) => ({ ...job, machine }));
          if (onObserveTransferJobs && entries.length > 0) {
            onObserveTransferJobs(entries);
          }
          setJobsByMachine((prev) => {
            const next = { ...(prev || {}) };
            for (const key of Object.keys(next)) {
              if (!activeSet.has(key)) delete next[key];
            }
            next[machine] = entries;
            return next;
          });
        } catch (err) {
          if (ctrl.signal.aborted) return;
          if (!firstError) firstError = err;
        }
      }),
    );

    if (ctrl.signal.aborted) return;
    if (!anySucceeded && firstError) {
      setError(firstError);
    }
    setJobsByMachine((prev) => (prev === null ? {} : prev));
  }

  useEffect(() => {
    load(targetNames);
    return () => {
      if (abortRef.current) {
        abortRef.current.abort();
        abortRef.current = null;
      }
    };
  }, [targetKey, localNode, route.jobId]);

  const jobs = useMemo(() => {
    if (jobsByMachine === null) return null;
    const flat = [];
    for (const list of Object.values(jobsByMachine)) {
      if (Array.isArray(list)) flat.push(...list);
    }
    return flat;
  }, [jobsByMachine]);

  // Poll live transfer jobs while visible so progress bars stay up to date.
  const hasRunningTransferJob = useMemo(
    () => Boolean(jobs?.some((j) => j.kind === 'transfer' && (j.state === 'running' || j.state === 'starting'))),
    [jobs],
  );

  useEffect(() => {
    if (!hasRunningTransferJob) return undefined;
    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') load(targetNames);
    }, 1000);
    return () => window.clearInterval(timer);
  }, [hasRunningTransferJob, targetKey]);

  async function handleStopJob(job) {
    setError(null);
    try {
      const on = targetFor(job.machine, localNode);
      await link.call('job.cancel', { id: job.id }, { on });
      await load();
      if (onRefreshFleet) onRefreshFleet();
    } catch (err) {
      setError(err);
    }
  }

  async function handleControlTransfer(id, action) {
    if (typeof link?.transferControl !== 'function') return;
    setError(null);
    try {
      await link.transferControl(id, action);
    } catch (err) {
      setError(err);
    }
  }

  const hasSelected = Boolean(route.machine && route.jobId);
  const selectedKey = hasSelected ? `${route.machine}:${route.jobId}` : '';

  const firstGroup = useMemo(() => {
    const combined = (jobs || []).map((j) => ({ _kind: 'job', ...j }));
    for (const t of deviceTransfers || []) {
      const mappedState =
        t.state === 'running' ||
        t.state === 'queued' ||
        t.state === 'paused' ||
        t.state === 'cancelling'
          ? 'running'
          : t.state === 'done'
            ? 'succeeded'
            : t.state === 'failed'
              ? 'failed'
              : 'cancelled';
      combined.push({ _kind: 'transfer', state: mappedState, created: t.created, finished: t.finished });
    }
    const groups = groupActivity(combined);
    return groups[0] || null;
  }, [jobs, deviceTransfers]);
  const firstGroupLabel = firstGroup?.label || 'Recent';
  const runningFact = firstGroup?.key === 'running' ? String(firstGroup.items.length) : null;

  const selectedJob = useMemo(() => {
    if (!hasSelected || !Array.isArray(jobs)) return null;
    return jobs.find((j) => j.machine === route.machine && j.id === route.jobId) || null;
  }, [hasSelected, jobs, route.machine, route.jobId]);

  const refreshActivityList = async () => {
    await Promise.all([
      load(targetNames),
      typeof link?.transfers === 'function' ? link.transfers().catch(() => {}) : Promise.resolve(),
      onRefreshFleet ? onRefreshFleet() : Promise.resolve(),
    ]);
  };

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function' || wide || hasSelected) return undefined;
    return onRegisterRefresh(refreshActivityList);
  }, [onRegisterRefresh, wide, hasSelected, targetKey, localNode]);

  const listPane = html`
    <section>
      ${error ? html`<${Notice} error=${error} />` : null}
      <${ActivityGroups}
        jobs=${jobs}
        deviceTransfers=${deviceTransfers}
        includeMachine=${true}
        technical=${technical}
        deviceNoun=${deviceNoun}
        canCancel=${canCancel}
        canControlTransfers=${canControlTransfers}
        selectedKey=${wide ? selectedKey : ''}
        hideFirstLabel=${wide && !error}
        hrefForJob=${(job) => routeHref({ section: 'activity', machine: job.machine, jobId: job.id })}
        onStopJob=${handleStopJob}
        onControlTransfer=${handleControlTransfer}
        onOpenTransfer=${onOpenTransfer}
      />
    </section>
  `;

  if (wide) {
    const selectedTitle = selectedJob
      ? technical
        ? selectedJob.command || selectedJob.name || selectedJob.id
        : friendlyJobName(selectedJob)
      : 'Activity';
    const selectedState = selectedJob ? friendlyJobState(selectedJob, technical) : null;

    return html`
      <${SplitView}
        leftTitle="Activity"
        leftLabel=${firstGroupLabel}
        leftLabelFact=${runningFact}
        leftBody=${listPane}
        onLeftRefresh=${refreshActivityList}
        rightTitle=${hasSelected ? selectedTitle : null}
        rightFact=${hasSelected && selectedState ? selectedState.label : null}
        rightLabel=${hasSelected ? 'Details' : null}
        rightLabelFact=${hasSelected ? route.machine : null}
        rightEmpty=${!hasSelected ? 'Choose an item on the left.' : null}
        rightBody=${hasSelected
          ? html`
              <${ActivityDetail}
                machineName=${route.machine}
                nodeName=${localNode}
                jobId=${route.jobId}
                link=${link}
                allowedSet=${allowedSet}
                technical=${technical}
                onObserveTransferJobs=${onObserveTransferJobs}
                onChanged=${() => {
                  load();
                  if (onRefreshFleet) onRefreshFleet();
                }}
              />
            `
          : null}
      />
    `;
  }

  if (hasSelected) {
    return html`
      <${ActivityDetail}
        machineName=${route.machine}
        nodeName=${localNode}
        jobId=${route.jobId}
        link=${link}
        allowedSet=${allowedSet}
        technical=${technical}
        onObserveTransferJobs=${onObserveTransferJobs}
        onRegisterRefresh=${onRegisterRefresh}
        onChanged=${() => {
          load();
          if (onRefreshFleet) onRefreshFleet();
        }}
      />
    `;
  }

  return listPane;
}
