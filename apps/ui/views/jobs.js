// Jobs screen: background jobs across all answering machines, running first.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Notice } from './common.js';
import { callWithTimeout, DETAIL_TIMEOUT_MS } from '../lib/fleet.js';
import { formatElapsed, formatRelativeTime } from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import { JobFollowView } from './machine.js';

function sortJobs(list) {
  return [...list].sort((a, b) => {
    const aRun = a.state === 'running' || a.state === 'starting' ? 0 : 1;
    const bRun = b.state === 'running' || b.state === 'starting' ? 0 : 1;
    if (aRun !== bRun) return aRun - bRun;
    return String(b.created || '').localeCompare(String(a.created || ''));
  });
}

export function JobsView({
  route,
  snapshot,
  link,
  nodeInfo,
  allowedSet,
  wide,
  onRefreshFleet,
}) {
  const [jobsByMachine, setJobsByMachine] = useState(null);
  const [error, setError] = useState(null);
  const abortRef = useRef(null);
  const localNode = nodeInfo?.node || 'this node';

  // Only query answering machines (plus this node); never wait on unreachable or asking peers.
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
        const on = machine === localNode ? undefined : machine;
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
    return sortJobs(flat);
  }, [jobsByMachine]);

  const running = (jobs || []).filter((j) => j.state === 'running' || j.state === 'starting');
  const finished = (jobs || []).filter((j) => j.state !== 'running' && j.state !== 'starting');

  function renderJobRow(job) {
    const active = job.state === 'running' || job.state === 'starting';
    const selected = route.machine === job.machine && route.jobId === job.id;
    const elapsed =
      active && job.created
        ? formatElapsed(Math.max(0, (Date.now() - Date.parse(job.created)) / 1000))
        : job.exit_code != null
          ? `exit ${job.exit_code}`
          : job.state;
    return html`
      <a
        key=${`${job.machine}:${job.id}`}
        class=${['row', selected ? 'selected' : ''].filter(Boolean).join(' ')}
        href=${routeHref({ section: 'jobs', machine: job.machine, jobId: job.id })}
        aria-current=${selected ? 'true' : undefined}
      >
        <span class=${active ? 'dot busy' : 'dot off'} aria-hidden="true"></span>
        <span class="body">
          <span class="title mono">${job.command || job.name || job.id}</span>
          <span class="sub">
            ${job.machine}
            ${job.created ? ` · ${active ? 'started ' : ''}${formatRelativeTime(job.created)}` : ''}
          </span>
        </span>
        <span class="end">${elapsed}</span>
      </a>
    `;
  }

  const listPane = html`
    <section>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${jobs === null
        ? html`<div class="empty">Loading jobs…</div>`
        : jobs.length === 0
          ? html`<div class="empty">No background jobs recorded.</div>`
          : html`
              ${running.length > 0
                ? html`
                    <p class="label">Running · ${running.length}</p>
                    <div class="list">${running.map(renderJobRow)}</div>
                  `
                : null}
              ${finished.length > 0
                ? html`
                    <p class="label">Recent</p>
                    <div class="list">${finished.map(renderJobRow)}</div>
                  `
                : null}
            `}
    </section>
  `;

  const hasSelected = Boolean(route.machine && route.jobId);

  if (wide) {
    return html`
      <div class="split">
        ${listPane}
        <section>
          ${hasSelected
            ? html`
                <${JobFollowView}
                  machineName=${route.machine}
                  nodeName=${localNode}
                  jobId=${route.jobId}
                  link=${link}
                  allowedSet=${allowedSet}
                  onChanged=${() => {
                    load();
                    if (onRefreshFleet) onRefreshFleet();
                  }}
                />
              `
            : html`<div class="empty">Select a job to follow its output.</div>`}
        </section>
      </div>
    `;
  }

  if (hasSelected) {
    return html`
      <${JobFollowView}
        machineName=${route.machine}
        nodeName=${localNode}
        jobId=${route.jobId}
        link=${link}
        allowedSet=${allowedSet}
        onBack=${() => {
          window.location.hash = '#/jobs';
        }}
        onChanged=${() => {
          load();
          if (onRefreshFleet) onRefreshFleet();
        }}
      />
    `;
  }

  return listPane;
}
