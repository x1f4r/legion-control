// Machines screen: fleet at a glance, running jobs across machines, and wide split detail.

import { html } from '../vendor/preact-htm.js';
import { Meter, Notice } from './common.js';
import {
  formatDuration,
  formatElapsed,
  formatRelativeTime,
  machineMetrics,
  osLabel,
} from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import { MachineDetail } from './machine.js';

function machineSubtext(m) {
  if (!m.reachable) {
    return m.reason || 'Did not answer';
  }
  const parts = [];
  if (m.os) parts.push(osLabel(m.os));
  if (m.arch) parts.push(m.arch);
  if (m.runningJobs > 0) {
    parts.push(`${m.runningJobs} ${m.runningJobs === 1 ? 'job' : 'jobs'} running`);
  } else if (m.busy && m.busyReasons?.length) {
    parts.push(`busy: ${m.busyReasons[0]}`);
  } else if (m.sys?.uptime_s != null) {
    parts.push(`up ${formatDuration(m.sys.uptime_s)}`);
  }
  return parts.join(' · ') || 'Answering';
}

function MachineRow({ machine, selected, canWake, onWake }) {
  const metrics = machineMetrics(machine.sys);
  const dotClass = !machine.reachable ? 'dot off' : machine.busy ? 'dot busy' : 'dot';
  const rowClass = ['row', !machine.reachable ? 'off' : '', selected ? 'selected' : '']
    .filter(Boolean)
    .join(' ');
  const href = routeHref({ section: 'machines', machine: machine.name, tab: 'overview' });

  function handleWake(e) {
    e.preventDefault();
    e.stopPropagation();
    if (onWake) onWake(machine);
  }

  return html`
    <a class=${rowClass} href=${href} aria-current=${selected ? 'true' : undefined}>
      <span class=${dotClass} aria-hidden="true"></span>
      <span class="body">
        <span class="title mono">${machine.name}</span>
        <span class="sub">${machineSubtext(machine)}</span>
      </span>
      ${machine.reachable && metrics
        ? html`
            <span
              class="meters"
              aria-label=${`Processor ${metrics.cpuPct}%, memory ${metrics.memPct}%, disk ${metrics.diskPct}%`}
            >
              <${Meter} value=${metrics.cpuPct} />
              <${Meter} value=${metrics.memPct} />
              <${Meter} value=${metrics.diskPct} />
            </span>
          `
        : !machine.reachable && machine.canWake && canWake
          ? html`<button type="button" class="chip chip-action" onClick=${handleWake}>wake</button>`
          : null}
    </a>
  `;
}

export function MachinesView({
  snapshot,
  error,
  loading,
  wide,
  route,
  link,
  nodeInfo,
  tools,
  allowedSet,
  onWakeMachine,
  onRequestConfirm,
  onRefresh,
  runHistory,
  onRecordRun,
}) {
  const machines = snapshot?.machines || [];
  const runningJobs = snapshot?.runningJobs || [];
  const answering = machines.filter((m) => m.reachable).length;
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;

  const selectedName = wide
    ? route.machine || machines[0]?.name || null
    : route.machine || null;
  const selectedMachine = machines.find((m) => m.name === selectedName) || null;

  const listColumn = html`
    <section>
      ${error ? html`<${Notice} error=${error} />` : null}
      <p class="label">
        ${machines.length > 0
          ? `Machines · ${answering} of ${machines.length} answering`
          : loading
            ? 'Machines · loading…'
            : 'Machines'}
      </p>
      ${machines.length > 0
        ? html`
            <div class="list">
              ${machines.map(
                (m) => html`
                  <${MachineRow}
                    key=${m.name}
                    machine=${m}
                    selected=${wide && m.name === selectedName}
                    canWake=${canWake}
                    onWake=${onWakeMachine}
                  />
                `,
              )}
            </div>
          `
        : !loading
          ? html`<div class="empty">No machines found yet.</div>`
          : null}

      ${runningJobs.length > 0
        ? html`
            <p class="label">Running now</p>
            <div class="list">
              ${runningJobs.map((job) => {
                const elapsed =
                  job.duration_s != null
                    ? formatElapsed(job.duration_s)
                    : job.created
                      ? formatElapsed(Math.max(0, (Date.now() - Date.parse(job.created)) / 1000))
                      : '';
                return html`
                  <a
                    key=${`${job.machine}:${job.id}`}
                    class="row"
                    href=${routeHref({
                      section: 'machines',
                      machine: job.machine,
                      tab: 'jobs',
                      jobId: job.id,
                    })}
                  >
                    <span class="body">
                      <span class="title mono">${job.command || job.name || job.id}</span>
                      <span class="sub">
                        ${job.machine}${job.created ? ` · started ${formatRelativeTime(job.created)}` : ''}
                      </span>
                    </span>
                    <span class="end">${elapsed}</span>
                  </a>
                `;
              })}
            </div>
          `
        : null}
    </section>
  `;

  if (wide) {
    return html`
      <div class="split">
        ${listColumn}
        <section>
          ${selectedMachine
            ? html`
                <${MachineDetail}
                  machine=${selectedMachine}
                  tab=${route.tab || 'overview'}
                  jobId=${route.jobId || null}
                  showPaneTitle=${true}
                  link=${link}
                  nodeInfo=${nodeInfo}
                  tools=${tools}
                  allowedSet=${allowedSet}
                  onRequestConfirm=${onRequestConfirm}
                  onRefreshFleet=${onRefresh}
                  runHistory=${runHistory}
                  onRecordRun=${onRecordRun}
                />
              `
            : html`<div class="empty">Select a machine to inspect it.</div>`}
        </section>
      </div>
    `;
  }

  if (route.machine && selectedMachine) {
    return html`
      <${MachineDetail}
        machine=${selectedMachine}
        tab=${route.tab || 'overview'}
        jobId=${route.jobId || null}
        showPaneTitle=${false}
        link=${link}
        nodeInfo=${nodeInfo}
        tools=${tools}
        allowedSet=${allowedSet}
        onRequestConfirm=${onRequestConfirm}
        onRefreshFleet=${onRefresh}
        runHistory=${runHistory}
        onRecordRun=${onRecordRun}
      />
    `;
  }

  return listColumn;
}
