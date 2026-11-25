// Machines screen: labelled metrics for on machines, calm Off section, and wide split detail.

import { html } from '../vendor/preact-htm.js';
import { Meter, Notice } from './common.js';
import { formatMachineStatusPhrase } from '../lib/activity.js';
import { machineMetrics } from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import { formatPairSyncNotice } from '../lib/settings.js';
import { MachineDetail } from './machine.js';

function OnlineMachineCard({ machine, selected, showMetrics }) {
  const metrics = machineMetrics(machine.sys);
  const dotClass = machine.busy ? 'dot busy' : 'dot';
  const cls = ['machine', selected ? 'selected' : ''].filter(Boolean).join(' ');
  const href = routeHref({ section: 'machines', machine: machine.name, tab: 'overview' });

  return html`
    <a class=${cls} href=${href} aria-current=${selected ? 'true' : undefined}>
      <header>
        <span class=${dotClass} aria-hidden="true"></span>
        <strong>${machine.name}</strong>
        <span>${formatMachineStatusPhrase(machine)}</span>
      </header>
      ${showMetrics && metrics
        ? html`
            <div class="stats">
              <div class="stat">
                <span>Processor <b>${metrics.cpuPct}%</b></span>
                <${Meter} value=${metrics.cpuPct} />
              </div>
              <div class="stat">
                <span>Memory <b>${metrics.memPct}%</b></span>
                <${Meter} value=${metrics.memPct} />
              </div>
              <div class="stat">
                <span>Disk <b>${metrics.diskPct}%</b></span>
                <${Meter} value=${metrics.diskPct} />
              </div>
            </div>
          `
        : null}
    </a>
  `;
}

function OfflineMachineRow({ machine, selected, canWake, wakePending, onWake }) {
  const isAsking = machine.status === 'asking' || machine.asking;
  const cls = [
    'machine',
    isAsking ? 'asking' : 'off',
    selected ? 'selected' : '',
  ]
    .filter(Boolean)
    .join(' ');
  const href = routeHref({ section: 'machines', machine: machine.name, tab: 'overview' });
  const showWake = !isAsking && !wakePending && machine.canWake && canWake;

  return html`
    <div class=${cls}>
      <a class="machine-link" href=${href} aria-current=${selected ? 'true' : undefined}>
        <span class=${isAsking ? 'dot asking' : 'dot off'} aria-hidden="true"></span>
        <strong>${machine.name}</strong>
        <span>${formatMachineStatusPhrase(machine, Date.now(), { wakePending })}</span>
      </a>
      ${showWake
        ? html`
            <button
              type="button"
              class="btn small"
              onClick=${(e) => {
                e.preventDefault();
                e.stopPropagation();
                if (onWake) onWake(machine);
              }}
            >
              Wake
            </button>
          `
        : null}
    </div>
  `;
}

export function MachinesView({
  snapshot,
  error,
  pairNotice,
  onDismissPairNotice,
  loading,
  wide,
  route,
  link,
  nodeInfo,
  tools,
  allowedSet,
  settings,
  deviceNoun,
  thumbCache,
  wakePendingSet,
  onWakeMachine,
  onWakeSent,
  onRequestConfirm,
  onRefresh,
  commandRuns,
  onCommandRunsChange,
}) {
  const machines = snapshot?.machines || [];
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;
  const showMetrics = settings?.showMetrics !== false;
  const showOffline = settings?.showOffline !== false;
  const syncBanner = formatPairSyncNotice(pairNotice, Boolean(settings?.technical));

  const onlineMachines = machines.filter((m) => m.reachable);
  const askingMachines = machines.filter((m) => !m.reachable && (m.status === 'asking' || m.asking));
  const offMachines = machines.filter((m) => !m.reachable && m.status !== 'asking' && !m.asking);

  const selectedName = wide
    ? route.machine || onlineMachines[0]?.name || machines[0]?.name || null
    : route.machine || null;
  const selectedMachine = machines.find((m) => m.name === selectedName) || null;

  const listColumn = html`
    <section>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${syncBanner
        ? html`
            <${Notice}
              title=${syncBanner.title}
              hint=${syncBanner.detail}
              kind=${syncBanner.kind}
              onDismiss=${onDismissPairNotice}
            />
          `
        : null}

      ${machines.length === 0
        ? loading
          ? html`<div class="empty">Checking machines…</div>`
          : html`<div class="empty">No machines configured yet.</div>`
        : html`
            ${onlineMachines.length > 0
              ? html`
                  <div class="list">
                    ${onlineMachines.map(
                      (m) => html`
                        <${OnlineMachineCard}
                          key=${m.name}
                          machine=${m}
                          selected=${wide && m.name === selectedName}
                          showMetrics=${showMetrics}
                        />
                      `,
                    )}
                  </div>
                `
              : null}

            ${askingMachines.length > 0
              ? html`
                  ${onlineMachines.length > 0 ? html`<p class="label">Checking</p>` : null}
                  <div class="list">
                    ${askingMachines.map(
                      (m) => html`
                        <${OfflineMachineRow}
                          key=${m.name}
                          machine=${m}
                          selected=${wide && m.name === selectedName}
                          canWake=${false}
                          onWake=${onWakeMachine}
                        />
                      `,
                    )}
                  </div>
                `
              : null}

            ${showOffline && offMachines.length > 0
              ? html`
                  <p class="label">Off</p>
                  <div class="list">
                    ${offMachines.map(
                      (m) => html`
                        <${OfflineMachineRow}
                          key=${m.name}
                          machine=${m}
                          selected=${wide && m.name === selectedName}
                          canWake=${canWake}
                          wakePending=${Boolean(wakePendingSet?.has(m.name))}
                          onWake=${onWakeMachine}
                        />
                      `,
                    )}
                  </div>
                `
              : null}
          `}
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
                  wide=${true}
                  link=${link}
                  nodeInfo=${nodeInfo}
                  tools=${tools}
                  allowedSet=${allowedSet}
                  settings=${settings}
                  deviceNoun=${deviceNoun}
                  thumbCache=${thumbCache}
                  wakePending=${Boolean(wakePendingSet?.has(selectedMachine.name))}
                  onWakeMachine=${onWakeMachine}
                  onWakeSent=${onWakeSent}
                  onRequestConfirm=${onRequestConfirm}
                  onRefreshFleet=${onRefresh}
                  commandRuns=${commandRuns}
                  onCommandRunsChange=${onCommandRunsChange}
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
        wide=${false}
        link=${link}
        nodeInfo=${nodeInfo}
        tools=${tools}
        allowedSet=${allowedSet}
        settings=${settings}
        deviceNoun=${deviceNoun}
        thumbCache=${thumbCache}
        wakePending=${Boolean(wakePendingSet?.has(selectedMachine.name))}
        onWakeMachine=${onWakeMachine}
        onWakeSent=${onWakeSent}
        onRequestConfirm=${onRequestConfirm}
        onRefreshFleet=${onRefresh}
        commandRuns=${commandRuns}
        onCommandRunsChange=${onCommandRunsChange}
      />
    `;
  }

  return listColumn;
}
