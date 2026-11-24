// Activity screen and job detail: plain by default, grouped by Running now / Today / Yesterday / Earlier.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Notice, SwipeRow } from './common.js';
import {
  formatActivityDuration,
  formatActivitySubtitle,
  formatFriendlyRelativeTime,
  friendlyJobName,
  friendlyJobState,
  groupActivity,
  jobDurationSeconds,
} from '../lib/activity.js';
import { callWithTimeout, DETAIL_TIMEOUT_MS } from '../lib/fleet.js';
import { routeHref } from '../lib/route.js';

function targetFor(machineName, nodeName) {
  return machineName && machineName !== nodeName ? machineName : undefined;
}

export function ActivityDetail({
  machineName,
  nodeName,
  jobId,
  link,
  allowedSet,
  technical = false,
  onChanged,
}) {
  const [status, setStatus] = useState(null);
  const [logText, setLogText] = useState('');
  const [error, setError] = useState(null);
  const [stopping, setStopping] = useState(false);
  const offsetRef = useRef(0);
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
        if (logs?.content) {
          setLogText((prev) => prev + logs.content);
        }
        if (typeof logs?.next_offset === 'number') {
          offsetRef.current = logs.next_offset;
        }
        if (st.state === 'starting' || st.state === 'running') {
          timer = window.setTimeout(poll, 900);
        }
      } catch (err) {
        if (alive) setError(err);
      }
    }

    poll();
    return () => {
      alive = false;
      if (timer) window.clearTimeout(timer);
    };
  }, [machineName, jobId]);

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
                  <span class=${`state ${stateInfo.cls}`}>${stateInfo.label}</span>
                </dd>
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
  const stateInfo = friendlyJobState(job, technical);
  const title = technical ? job.command || job.name || job.id : friendlyJobName(job);
  const subtitle = formatActivitySubtitle(job, { includeMachine, technical });
  const rowCls = ['row', selected ? 'selected' : ''].filter(Boolean).join(' ');

  const rowEl = html`
    <a class=${rowCls} href=${href} aria-current=${selected ? 'true' : undefined}>
      <span class="body">
        <span class=${technical ? 'title mono' : 'title'}>${title}</span>
        ${subtitle ? html`<span class="sub">${subtitle}</span>` : null}
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

export function ActivityGroups({
  jobs,
  includeMachine = true,
  technical = false,
  canCancel = false,
  selectedKey = '',
  hrefForJob,
  onStopJob,
  emptyText = 'Nothing has run yet. Things you start here or on a machine appear in this list.',
}) {
  const groups = useMemo(() => groupActivity(jobs || []), [jobs]);

  if (jobs === null) {
    return html`<div class="empty">Loading…</div>`;
  }
  if (groups.length === 0) {
    return html`<div class="empty">${emptyText}</div>`;
  }

  return html`
    <div>
      ${groups.map(
        (group) => html`
          <div key=${group.key}>
            <p class="label">${group.label}</p>
            <div class="list">
              ${group.items.map((job) => {
                const key = `${job.machine || ''}:${job.id}`;
                return html`
                  <${ActivityRow}
                    key=${key}
                    job=${job}
                    href=${hrefForJob(job)}
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
  onRefreshFleet,
}) {
  const [jobsByMachine, setJobsByMachine] = useState(null);
  const [error, setError] = useState(null);
  const abortRef = useRef(null);
  const localNode = nodeInfo?.node || 'this node';
  const technical = Boolean(settings?.technical);
  const canCancel = allowedSet ? allowedSet.has('job.cancel') : true;

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

  const hasSelected = Boolean(route.machine && route.jobId);
  const selectedKey = hasSelected ? `${route.machine}:${route.jobId}` : '';

  const listPane = html`
    <section>
      ${error ? html`<${Notice} error=${error} />` : null}
      <${ActivityGroups}
        jobs=${jobs}
        includeMachine=${true}
        technical=${technical}
        canCancel=${canCancel}
        selectedKey=${wide ? selectedKey : ''}
        hrefForJob=${(job) => routeHref({ section: 'activity', machine: job.machine, jobId: job.id })}
        onStopJob=${handleStopJob}
      />
    </section>
  `;

  if (wide && (hasSelected || !jobs || jobs.length > 0)) {
    return html`
      <div class="split">
        ${listPane}
        <section>
          ${hasSelected
            ? html`
                <${ActivityDetail}
                  machineName=${route.machine}
                  nodeName=${localNode}
                  jobId=${route.jobId}
                  link=${link}
                  allowedSet=${allowedSet}
                  technical=${technical}
                  onChanged=${() => {
                    load();
                    if (onRefreshFleet) onRefreshFleet();
                  }}
                />
              `
            : html`<div class="empty">Select an item to see its details.</div>`}
        </section>
      </div>
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
        onChanged=${() => {
          load();
          if (onRefreshFleet) onRefreshFleet();
        }}
      />
    `;
  }

  return listPane;
}
