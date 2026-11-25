// Machines screen: labelled metrics for on machines, calm Off section, and wide split detail.

import { html } from '../vendor/preact-htm.js';
import { Meter, Notice } from './common.js';
import {
  formatMachineDetailSubtitle,
  formatMachinesShortFact,
  formatMachineStatusPhrase,
} from '../lib/activity.js';
import { machineMetrics } from '../lib/format.js';
import {
  buildOverviewMeters,
  formatOverviewStateLine,
  isOverviewBusy,
  machineAttentionLabel,
} from '../lib/machine-monitor.js';
import { routeHref, WIDE_MACHINE_TABS } from '../lib/route.js';
import { formatPairSyncNotice } from '../lib/settings.js';
import { MachineDetail, WIDE_TAB_LABELS } from './machine.js';
import { SplitView } from './split-view.js';

function OnlineMachineCard({ machine, selected, showMetrics }) {
  const metrics = machineMetrics(machine.sys);
  const attention = machineAttentionLabel(machine.sys);
  const dotClass = machine.busy ? 'dot busy' : 'dot';
  const cls = ['machine', selected ? 'selected' : ''].filter(Boolean).join(' ');
  const href = routeHref({ section: 'machines', machine: machine.name, tab: 'overview' });
  const hasCpu = Number.isFinite(machine.sys?.cpu?.usage_pct);
  const hasMem = Number.isFinite(machine.sys?.memory?.total) && machine.sys.memory.total > 0;
  const hasDisk = Boolean(metrics?.disk);

  return html`
    <a class=${cls} href=${href} aria-current=${selected ? 'true' : undefined}>
      <header>
        <span class=${dotClass} aria-hidden="true"></span>
        <strong>${machine.name}</strong>
        <span>${formatMachineStatusPhrase(machine)}</span>
      </header>
      ${attention ? html`<div class="attention-word">${attention}</div>` : null}
      ${showMetrics && metrics && (hasCpu || hasMem || hasDisk)
        ? html`
            <div class="stats">
              ${hasCpu
                ? html`
                    <div class="stat">
                      <span>Processor <b>${metrics.cpuPct}%</b></span>
                      <${Meter} value=${metrics.cpuPct} />
                    </div>
                  `
                : null}
              ${hasMem
                ? html`
                    <div class="stat">
                      <span>Memory <b>${metrics.memPct}%</b></span>
                      <${Meter} value=${metrics.memPct} />
                    </div>
                  `
                : null}
              ${hasDisk
                ? html`
                    <div class="stat">
                      <span>Storage <b>${metrics.diskPct}%</b></span>
                      <${Meter} value=${metrics.diskPct} />
                    </div>
                  `
                : null}
            </div>
          `
        : null}
    </a>
  `;
}

function OfflineMachineRow({ machine, selected, canWake, wakePending, onEnsureAwake }) {
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

  function handleOpen(e) {
    if (!isAsking && !machine.reachable && machine.canWake && canWake && onEnsureAwake) {
      e.preventDefault();
      onEnsureAwake(machine, {
        kind: 'screen',
        label: `open ${machine.name}`,
        onContinue: () => {
          window.location.hash = href;
        },
      });
    }
  }

  return html`
    <div class=${cls}>
      <a class="machine-link" href=${href} aria-current=${selected ? 'true' : undefined} onClick=${handleOpen}>
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
                if (onEnsureAwake) {
                  onEnsureAwake(machine, { kind: 'wake', immediate: true, forgettable: true });
                }
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
  updateState,
  onOpenWhatIsNew,
  onDismissUpdateNotice,
  loading,
  wide,
  route,
  link,
  capsStore = null,
  folderChanges = null,
  onObserveTransferJobs = null,
  onRegisterRefresh = null,
  nodeInfo,
  tools,
  allowedSet,
  settings,
  deviceNoun,
  thumbCache,
  filesState = null,
  filesActions = null,
  wakePendingSet,
  onEnsureAwake,
  onWakeSent,
  onRequestConfirm,
  onRefresh,
  onUpdateMachineSys,
  commandRuns,
  onCommandRunsChange,
}) {
  const machines = snapshot?.machines || [];
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;
  const showMetrics = settings?.showMetrics !== false;
  const showOffline = settings?.showOffline !== false;
  const syncBanner = formatPairSyncNotice(pairNotice, Boolean(settings?.technical));
  const justUpdated = updateState?.just_updated || null;
  const rolledBack = updateState?.rolled_back || null;

  const onlineMachines = machines.filter((m) => m.reachable);
  const askingMachines = machines.filter((m) => !m.reachable && (m.status === 'asking' || m.asking));
  const offMachines = machines.filter((m) => !m.reachable && m.status !== 'asking' && !m.asking);

  const selectedName = wide
    ? route.machine || onlineMachines[0]?.name || machines[0]?.name || null
    : route.machine || null;
  const selectedMachine = machines.find((m) => m.name === selectedName) || null;
  const hasTopBanner = Boolean(error || rolledBack || justUpdated || syncBanner);
  const firstLeftGroup =
    onlineMachines.length > 0
      ? 'online'
      : askingMachines.length > 0
        ? 'asking'
        : showOffline && offMachines.length > 0
          ? 'off'
          : 'empty';
  const hideFirstLeftGroupLabel = wide && !hasTopBanner;

  const listColumn = html`
    <section>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${rolledBack
        ? html`
            <${Notice}
              title=${`Kept version ${updateState?.current?.version || ''}`.trim()}
              hint=${rolledBack.reason ||
                `Version ${rolledBack.version} did not start cleanly, so Legion kept the previous copy.`}
              kind="warn"
              onDismiss=${onDismissUpdateNotice}
            />
          `
        : null}
      ${justUpdated
        ? html`
            <div class="update-note" role="status">
              ${justUpdated.notes
                ? html`
                    <button type="button" class="update-note-link" onClick=${onOpenWhatIsNew}>
                      <span>Updated to ${justUpdated.version}</span>
                      <span aria-hidden="true">·</span>
                      <span class="update-note-action">What is new</span>
                    </button>
                  `
                : html`<span class="update-note-text">Updated to ${justUpdated.version}</span>`}
              <button
                type="button"
                class="update-note-dismiss"
                aria-label="Dismiss update notice"
                onClick=${onDismissUpdateNotice}
              >
                ×
              </button>
            </div>
          `
        : null}
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
                        />
                      `,
                    )}
                  </div>
                `
              : null}

            ${showOffline && offMachines.length > 0
              ? html`
                  ${!(hideFirstLeftGroupLabel && firstLeftGroup === 'off')
                    ? html`<p class="label">Off</p>`
                    : null}
                  <div class="list">
                    ${offMachines.map(
                      (m) => html`
                        <${OfflineMachineRow}
                          key=${m.name}
                          machine=${m}
                          selected=${wide && m.name === selectedName}
                          canWake=${canWake}
                          wakePending=${Boolean(wakePendingSet?.has(m.name))}
                          onEnsureAwake=${onEnsureAwake}
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
    const leftLabel =
      firstLeftGroup === 'online'
        ? 'On'
        : firstLeftGroup === 'asking'
          ? 'Checking'
          : firstLeftGroup === 'off'
            ? 'Off'
            : 'Machines';
    const leftLabelFact = formatMachinesShortFact(machines, {
      cachedAt: snapshot?.cachedAt ?? null,
    });

    const effectiveTab = route.tab && route.tab !== 'more' ? route.tab : 'overview';
    const selectedWakePending = selectedMachine ? Boolean(wakePendingSet?.has(selectedMachine.name)) : false;
    const dotClass = selectedMachine
      ? selectedMachine.status === 'asking' || selectedMachine.asking
        ? 'dot asking'
        : !selectedMachine.reachable
          ? 'dot off'
          : selectedMachine.busy
            ? 'dot busy'
            : 'dot'
      : 'dot off';

    let rightLabel = null;
    let rightLabelFact = null;
    if (selectedMachine) {
      if (effectiveTab === 'overview' && selectedMachine.reachable) {
        const overviewMeters = buildOverviewMeters(selectedMachine.sys, {
          technical: Boolean(settings?.technical),
        });
        const overviewBusy = isOverviewBusy(selectedMachine.sys);
        const stateLine = formatOverviewStateLine(selectedMachine.sys);
        rightLabel = overviewMeters.length > 0 ? 'Health' : 'Overview';
        rightLabelFact = html`
          <span class="machine-state">
            <span class=${overviewBusy ? 'dot busy' : 'dot'} aria-hidden="true"></span>
            <span>${stateLine}</span>
          </span>
        `;
      } else {
        rightLabel = selectedMachine.reachable ? WIDE_TAB_LABELS[effectiveTab] || 'Overview' : 'Status';
        const detailSub = formatMachineDetailSubtitle(selectedMachine, Date.now(), {
          wakePending: selectedWakePending,
        });
        rightLabelFact = detailSub
          ? html`<span class="machine-state"><span>${detailSub}</span></span>`
          : null;
      }
    }

    return html`
      <${SplitView}
        leftTitle="Machines"
        leftLabel=${leftLabel}
        leftLabelFact=${leftLabelFact || null}
        leftBody=${listColumn}
        onLeftRefresh=${onRefresh}
        rightTitle=${selectedMachine
          ? html`
              <span class=${dotClass} aria-hidden="true"></span>
              <span class="pane-title-text">${selectedMachine.name}</span>
            `
          : null}
        rightTools=${selectedMachine
          ? html`
              <div class="tabs" role="tablist" aria-label=${`${selectedMachine.name} sections`}>
                ${WIDE_MACHINE_TABS.map(
                  (id) => html`
                    <button
                      type="button"
                      key=${id}
                      role="tab"
                      aria-selected=${effectiveTab === id ? 'true' : 'false'}
                      onClick=${() => {
                        window.location.hash = routeHref({
                          section: 'machines',
                          machine: selectedMachine.name,
                          tab: id,
                        });
                      }}
                    >
                      ${WIDE_TAB_LABELS[id]}
                    </button>
                  `,
                )}
              </div>
            `
          : null}
        rightLabel=${rightLabel}
        rightLabelFact=${rightLabelFact}
        rightEmpty=${!selectedMachine ? 'Choose a machine on the left.' : null}
        rightBody=${selectedMachine
          ? html`
              <${MachineDetail}
                machine=${selectedMachine}
                tab=${route.tab || 'overview'}
                jobId=${route.jobId || null}
                wide=${true}
                hideWideChrome=${true}
                link=${link}
                capsStore=${capsStore}
                folderChanges=${folderChanges}
                onObserveTransferJobs=${onObserveTransferJobs}
                onRegisterRefresh=${null}
                nodeInfo=${nodeInfo}
                tools=${tools}
                allowedSet=${allowedSet}
                settings=${settings}
                deviceNoun=${deviceNoun}
                thumbCache=${thumbCache}
                filesState=${filesState}
                filesActions=${filesActions}
                wakePending=${selectedWakePending}
                onEnsureAwake=${onEnsureAwake}
                onWakeSent=${onWakeSent}
                onRequestConfirm=${onRequestConfirm}
                onRefreshFleet=${onRefresh}
                onUpdateMachineSys=${onUpdateMachineSys}
                commandRuns=${commandRuns}
                onCommandRunsChange=${onCommandRunsChange}
              />
            `
          : null}
      />
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
        capsStore=${capsStore}
        folderChanges=${folderChanges}
        onObserveTransferJobs=${onObserveTransferJobs}
        onRegisterRefresh=${onRegisterRefresh}
        nodeInfo=${nodeInfo}
        tools=${tools}
        allowedSet=${allowedSet}
        settings=${settings}
        deviceNoun=${deviceNoun}
        thumbCache=${thumbCache}
        filesState=${filesState}
        filesActions=${filesActions}
        wakePending=${Boolean(wakePendingSet?.has(selectedMachine.name))}
        onEnsureAwake=${onEnsureAwake}
        onWakeSent=${onWakeSent}
        onRequestConfirm=${onRequestConfirm}
        onRefreshFleet=${onRefresh}
        onUpdateMachineSys=${onUpdateMachineSys}
        commandRuns=${commandRuns}
        onCommandRunsChange=${onCommandRunsChange}
      />
    `;
  }

  return listColumn;
}
