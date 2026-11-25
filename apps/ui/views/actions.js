// Actions screen: plain-word tiles grouped by task, Which machine? step, and technical All tools door.

import { html, useMemo, useState } from '../vendor/preact-htm.js';
import { buildActionGroups, findActionById } from '../lib/actions.js';
import { formatMachineStatusPhrase } from '../lib/activity.js';
import { routeHref } from '../lib/route.js';
import { describePowerConfirm } from '../lib/schema.js';
import { Notice } from './common.js';
import { ToolDetail, ToolsView } from './tools.js';

function ActionDetail({
  action,
  snapshot,
  nodeInfo,
  link,
  allowedSet,
  technical,
  onRequestConfirm,
  onEnsureAwake,
  onWakeSent,
  onRefreshFleet,
}) {
  const [pickedMachine, setPickedMachine] = useState(null);
  const [resultMsg, setResultMsg] = useState(null);
  const [error, setError] = useState(null);
  const machines = snapshot?.machines || [];
  const localNode = nodeInfo?.node || 'this node';
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;

  if (!action) {
    return html`<div class="empty">That action is not available here.</div>`;
  }

  if (!action.allowed) {
    return html`
      <${Notice}
        kind="warn"
        title=${`${action.title} is not allowed on this device.`}
        hint=${action.reason}
      />
    `;
  }

  // Machine-tab actions (Browse, Command, Projects, Services): pick which answering (or off wakeable) machine, then open its tab.
  if (action.mode === 'machine-tab') {
    const candidates = machines.filter((m) => m.reachable);
    const wakeableOff = canWake ? machines.filter((m) => !m.reachable && m.canWake) : [];
    const wakeKind =
      action.machineTab === 'files'
        ? 'files'
        : action.machineTab === 'command'
          ? 'command'
          : 'action';
    return html`
      <div>
        <p class="lede">Choose a machine for ${action.title.toLowerCase()}.</p>
        ${candidates.length === 0 && wakeableOff.length === 0
          ? html`<div class="empty">No machines are on right now.</div>`
          : html`
              ${candidates.length > 0
                ? html`
                    <p class="label">Which machine?</p>
                    <div class="list">
                      ${candidates.map(
                        (m) => html`
                          <a
                            key=${m.name}
                            class="row"
                            href=${routeHref({
                              section: 'machines',
                              machine: m.name,
                              tab: action.machineTab,
                            })}
                          >
                            <span class=${m.busy ? 'dot busy' : 'dot'} aria-hidden="true"></span>
                            <span class="body">
                              <span class="title mono">${m.name}</span>
                              <span class="sub">${formatMachineStatusPhrase(m)}</span>
                            </span>
                            <span class="chev" aria-hidden="true">›</span>
                          </a>
                        `,
                      )}
                    </div>
                  `
                : null}
              ${wakeableOff.length > 0
                ? html`
                    <p class="label">Off</p>
                    <div class="list">
                      ${wakeableOff.map((m) => {
                        const destHref = routeHref({
                          section: 'machines',
                          machine: m.name,
                          tab: action.machineTab,
                        });
                        return html`
                          <button
                            type="button"
                            key=${m.name}
                            class="row off"
                            onClick=${() => {
                              if (onEnsureAwake) {
                                const continueLabel =
                                  action.machineTab === 'files'
                                    ? 'open its files'
                                    : action.machineTab === 'command'
                                      ? `open Command on ${m.name}`
                                      : `open ${action.title.toLowerCase()} on ${m.name}`;
                                onEnsureAwake(m, {
                                  kind: wakeKind,
                                  label: continueLabel,
                                  onContinue: () => {
                                    window.location.hash = destHref;
                                  },
                                });
                              } else {
                                window.location.hash = destHref;
                              }
                            }}
                          >
                            <span class="dot off" aria-hidden="true"></span>
                            <span class="body">
                              <span class="title mono">${m.name}</span>
                              <span class="sub">${formatMachineStatusPhrase(m)}</span>
                            </span>
                            <span class="chev" aria-hidden="true">›</span>
                          </button>
                        `;
                      })}
                    </div>
                  `
                : null}
            `}
      </div>
    `;
  }

  // Wake: pick an off wakeable machine and confirm through the shared wake sheet.
  if (action.id === 'wake') {
    const wakeable = machines.filter((m) => !m.reachable && m.canWake);
    return html`
      <div>
        ${error ? html`<${Notice} error=${error} />` : null}
        ${resultMsg ? html`<div class="notice"><strong>${resultMsg}</strong></div>` : null}
        ${wakeable.length === 0
          ? html`<div class="empty">No off machines with Wake-on-LAN right now.</div>`
          : html`
              <p class="label">Which machine?</p>
              <div class="list">
                ${wakeable.map(
                  (m) => html`
                    <button
                      type="button"
                      key=${m.name}
                      class="row"
                      onClick=${() => {
                        if (onEnsureAwake) {
                          onEnsureAwake(m, {
                            kind: 'wake',
                            immediate: true,
                            forgettable: true,
                            onContinue: () => {
                              setResultMsg(`${m.name} is on.`);
                              if (onRefreshFleet) onRefreshFleet();
                            },
                          });
                        }
                      }}
                    >
                      <span class="dot off" aria-hidden="true"></span>
                      <span class="body">
                        <span class="title mono">${m.name}</span>
                        <span class="sub">${formatMachineStatusPhrase(m)}</span>
                      </span>
                      <span class="btn small">Wake</span>
                    </button>
                  `,
                )}
              </div>
            `}
      </div>
    `;
  }

  // On-machine power / tool actions (Sleep, Restart, Shut down, Update Legion).
  if (action.targetFilter === 'on' && !pickedMachine) {
    const candidates = machines.filter((m) => m.reachable);
    const isPower = action.tool.startsWith('power.');
    const wakeableOff = !isPower && canWake ? machines.filter((m) => !m.reachable && m.canWake) : [];
    return html`
      <div>
        ${error ? html`<${Notice} error=${error} />` : null}
        ${resultMsg ? html`<div class="notice"><strong>${resultMsg}</strong></div>` : null}
        <p class="lede">${action.summary}.</p>
        ${candidates.length === 0 && wakeableOff.length === 0
          ? html`<div class="empty">No machines are on right now.</div>`
          : html`
              ${candidates.length > 0
                ? html`
                    <p class="label">Which machine?</p>
                    <div class="list">
                      ${candidates.map(
                        (m) => html`
                          <button
                            type="button"
                            key=${m.name}
                            class="row"
                            onClick=${() => {
                              if (isPower && action.tool !== 'power.restart') {
                                const on = m.name;
                                const liveJobs = Array.isArray(m.jobs) ? m.jobs : [];
                                const copy = describePowerConfirm(
                                  action.tool,
                                  m.name,
                                  {},
                                  liveJobs,
                                  m.busyReasons || [],
                                );
                                onRequestConfirm({
                                  title: copy.title,
                                  summary: copy.summary,
                                  jobs: liveJobs,
                                  tool: action.tool,
                                  targetName: m.name,
                                  args: {},
                                  effect: 'power',
                                  confirmLabel: copy.confirmLabel,
                                  canDryRun: true,
                                  async onDryRun() {
                                    return link.call(action.tool, { dry_run: true }, { on });
                                  },
                                  async onConfirm() {
                                    setError(null);
                                    try {
                                      await link.call(action.tool, { dry_run: false }, { on });
                                      setResultMsg(`${action.title} scheduled on ${m.name}.`);
                                      if (onRefreshFleet) onRefreshFleet();
                                    } catch (err) {
                                      setError(err);
                                    }
                                  },
                                });
                              } else if (action.tool === 'power.restart') {
                                window.location.hash = routeHref({
                                  section: 'machines',
                                  machine: m.name,
                                  tab: 'power',
                                });
                              } else {
                                setPickedMachine(m.name);
                              }
                            }}
                          >
                            <span class=${m.busy ? 'dot busy' : 'dot'} aria-hidden="true"></span>
                            <span class="body">
                              <span class="title mono">${m.name}</span>
                              <span class="sub">${formatMachineStatusPhrase(m)}</span>
                            </span>
                            <span class="chev" aria-hidden="true">›</span>
                          </button>
                        `,
                      )}
                    </div>
                  `
                : null}
              ${wakeableOff.length > 0
                ? html`
                    <p class="label">Off</p>
                    <div class="list">
                      ${wakeableOff.map(
                        (m) => html`
                          <button
                            type="button"
                            key=${m.name}
                            class="row off"
                            onClick=${() => {
                              if (onEnsureAwake) {
                                onEnsureAwake(m, {
                                  kind: 'action',
                                  label: `run ${action.title}`,
                                  onContinue: () => setPickedMachine(m.name),
                                });
                              } else {
                                setPickedMachine(m.name);
                              }
                            }}
                          >
                            <span class="dot off" aria-hidden="true"></span>
                            <span class="body">
                              <span class="title mono">${m.name}</span>
                              <span class="sub">${formatMachineStatusPhrase(m)}</span>
                            </span>
                            <span class="chev" aria-hidden="true">›</span>
                          </button>
                        `,
                      )}
                    </div>
                  `
                : null}
            `}
      </div>
    `;
  }

  // Tool runner (either no target needed, or target picked).
  if (!action.spec) {
    return html`<div class="empty">This tool is not available on this node.</div>`;
  }

  return html`
    <div>
      ${pickedMachine
        ? html`
            <div class="result-head">
              <p class="label">On ${pickedMachine}</p>
              <button
                type="button"
                class="btn small quiet"
                onClick=${() => setPickedMachine(null)}
              >
                Choose another machine
              </button>
            </div>
          `
        : null}
      <${ToolDetail}
        key=${`${action.spec.name}:${pickedMachine || 'local'}`}
        tool=${action.spec}
        allowed=${action.allowed}
        initialTarget=${pickedMachine || 'local'}
        nodeInfo=${nodeInfo}
        machines=${machines}
        runningJobs=${snapshot?.runningJobs || []}
        link=${link}
        technical=${technical}
        onRequestConfirm=${onRequestConfirm}
        onEnsureAwake=${onEnsureAwake}
      />
    </div>
  `;
}

export function ActionsView({
  route,
  tools,
  allowedSet,
  nodeInfo,
  snapshot,
  link,
  settings,
  wide,
  query,
  onRequestConfirm,
  onEnsureAwake,
  onWakeSent,
  onRefreshFleet,
}) {
  const technical = Boolean(settings?.technical);

  if (route.allTools) {
    return html`
      <${ToolsView}
        route=${route}
        tools=${tools}
        allowedSet=${allowedSet}
        nodeInfo=${nodeInfo}
        snapshot=${snapshot}
        link=${link}
        wide=${wide}
        technical=${technical}
        onRequestConfirm=${onRequestConfirm}
        onEnsureAwake=${onEnsureAwake}
      />
    `;
  }

  const groups = useMemo(
    () =>
      buildActionGroups({
        tools,
        allowedSet,
        whoami: nodeInfo?.whoami,
        query,
        technical,
      }),
    [tools, allowedSet, nodeInfo?.whoami, query, technical],
  );

  if (route.action) {
    const action = findActionById(route.action, tools, allowedSet, nodeInfo?.whoami);
    return html`
      <${ActionDetail}
        key=${route.action}
        action=${action}
        snapshot=${snapshot}
        nodeInfo=${nodeInfo}
        link=${link}
        allowedSet=${allowedSet}
        technical=${technical}
        onRequestConfirm=${onRequestConfirm}
        onEnsureAwake=${onEnsureAwake}
        onWakeSent=${onWakeSent}
        onRefreshFleet=${onRefreshFleet}
      />
    `;
  }

  return html`
    <div>
      ${groups.length === 0
        ? query && query.trim()
          ? html`<div class="empty">Nothing matches "${query.trim()}".</div>`
          : html`<div class="empty">This device is not allowed to run actions here.</div>`
        : groups.map(
            (group) => html`
              <div key=${group.group}>
                <p class="label">${group.group}</p>
                <div class="tiles">
                  ${group.items.map((item) => {
                    if (!item.allowed) {
                      return html`
                        <div key=${item.id} class="tile na" aria-disabled="true">
                          <strong>${item.title}</strong>
                          <span>${item.reason}</span>
                          ${technical ? html`<span class="tile-tool">${item.tool}</span>` : null}
                        </div>
                      `;
                    }
                    return html`
                      <a
                        key=${item.id}
                        class="tile"
                        href=${routeHref({ section: 'actions', action: item.id })}
                      >
                        <strong>${item.title}</strong>
                        <span>${item.summary}</span>
                        ${technical ? html`<span class="tile-tool">${item.tool}</span>` : null}
                      </a>
                    `;
                  })}
                </div>
              </div>
            `,
          )}

      ${technical
        ? html`
            <p class="label">Technical</p>
            <div class="list">
              <a class="row" href=${routeHref({ section: 'actions', allTools: true })}>
                <span class="body">
                  <span class="title">All tools</span>
                  <span class="sub">
                    ${`${(tools || []).length} tools from ${nodeInfo?.node || 'this node'}`}
                  </span>
                </span>
                <span class="chev" aria-hidden="true">›</span>
              </a>
            </div>
          `
        : null}
    </div>
  `;
}
