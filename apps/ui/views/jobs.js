// Jobs screen: background jobs across all answering machines, running first.

import { html, useEffect, useState } from '../vendor/preact-htm.js';
import { Notice } from './common.js';
import { formatElapsed, formatRelativeTime } from '../lib/format.js';
import { isFanOut, fanOutEntries } from '../lib/result.js';
import { routeHref } from '../lib/route.js';
import { JobFollowView } from './machine.js';

export function JobsView({
  route,
  snapshot,
  link,
  nodeInfo,
  allowedSet,
  wide,
  onRefreshFleet,
}) {
  const [jobs, setJobs] = useState(null);
  const [error, setError] = useState(null);
  const hasRemote = (snapshot?.machines || []).length > 1;
  const localNode = nodeInfo?.node || 'this node';

  async function load() {
    setError(null);
    try {
      const res = await link.call(
        'job.list',
        { limit: 50 },
        { on: hasRemote ? 'all' : undefined },
      );
      const collected = [];
      if (isFanOut(res)) {
        for (const entry of fanOutEntries(res)) {
          if (!entry.ok) continue;
          for (const job of entry.value?.jobs || []) {
            collected.push({ ...job, machine: entry.machine });
          }
        }
      } else {
        for (const job of res?.jobs || []) {
          collected.push({ ...job, machine: localNode });
        }
      }
      collected.sort((a, b) => {
        const aRun = a.state === 'running' || a.state === 'starting' ? 0 : 1;
        const bRun = b.state === 'running' || b.state === 'starting' ? 0 : 1;
        if (aRun !== bRun) return aRun - bRun;
        return String(b.created || '').localeCompare(String(a.created || ''));
      });
      setJobs(collected);
    } catch (err) {
      setError(err);
    }
  }

  useEffect(() => {
    load();
  }, [hasRemote, localNode, route.jobId]);

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
          ? html`<div class="empty">No background jobs yet.</div>`
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
