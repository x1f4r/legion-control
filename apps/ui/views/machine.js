// One machine screen: Overview, Files, Power, More, Command, Services, Projects, and Activity.

import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { Meter, Notice, OlderMachineNote, Sheet } from './common.js';
import { ActivityDetail, ActivityGroups, ActivityRow } from './activity.js';
import { MachineFilesBrowser } from './files.js';
import { plainDenyReason } from '../lib/actions.js';
import {
  formatCommandMeta,
  formatFriendlyRelativeTime,
  formatFriendlyUptime,
  formatMachineDetailSubtitle,
  friendlyJobName,
} from '../lib/activity.js';
import {
  formatBusyReasonLines,
  formatBytes,
  osLabel,
} from '../lib/format.js';
import {
  buildOverviewMeters,
  createOverviewPoller,
  formatOverviewStateLine,
  isOverviewBusy,
  selectBusyPrograms,
} from '../lib/machine-monitor.js';
import { routeHref, WIDE_MACHINE_TABS } from '../lib/route.js';
import { describePowerConfirm, unavailableReason } from '../lib/schema.js';

export const WIDE_TAB_LABELS = {
  overview: 'Overview',
  files: 'Files',
  power: 'Power',
  command: 'Command',
  services: 'Services',
  projects: 'Projects',
  activity: 'Activity',
};

function targetFor(machineName, nodeName) {
  return machineName || nodeName || undefined;
}

// ── Program detail and Stop / Force stop sheet ───────────────────────────────────────────────

function ProgramSheet({ program, machine, on, link, capsStore = null, canKill, technical, onStopped, onClose }) {
  const [step, setStep] = useState('detail'); // 'detail' | 'confirm' | 'stopping' | 'still_running' | 'refused'
  const [stillTargets, setStillTargets] = useState([]);
  const [refusals, setRefusals] = useState([]);
  const [error, setError] = useState(null);

  if (!program) return null;

  const isSelf = Boolean(program.self);
  const title = program.count > 1 ? `${program.name} × ${program.count}` : program.name;
  const members = Array.isArray(program.members) ? program.members : [];
  const pidList = members.map((m) => m.pid).filter((p) => Number.isFinite(p));
  const verifiedTargets = members
    .filter((m) => Number.isFinite(m.pid) && typeof m.start_time === 'string' && m.start_time)
    .map((m) => ({ pid: m.pid, start_time: m.start_time }));

  async function sendStop(targets, force) {
    setStep('stopping');
    setError(null);
    setRefusals([]);
    try {
      const args =
        targets.length > 0
          ? { targets, force }
          : { pid: pidList[0], force };
      const res = capsStore
        ? (await capsStore.call('sys.kill', args, { on })).value
        : await link.call('sys.kill', args, { on });
      const items = Array.isArray(res?.results) ? res.results : [];
      if (items.length === 0) {
        if (res?.stopped === false) {
          setStillTargets(targets);
          setStep('still_running');
        } else {
          if (onStopped) onStopped();
          onClose();
        }
        return;
      }

      const targetByPid = new Map(targets.map((t) => [t.pid, t]));
      const remaining = [];
      const refusedMsgs = [];
      const seenMsg = new Set();
      for (const item of items) {
        if (item.status === 'still_running') {
          const matched = targetByPid.get(item.pid);
          if (matched) remaining.push(matched);
        } else if (item.status === 'refused') {
          const msg = item.message || 'This program could not be stopped.';
          if (!seenMsg.has(msg)) {
            seenMsg.add(msg);
            refusedMsgs.push(msg);
          }
        }
      }

      if (remaining.length > 0) {
        setStillTargets(remaining);
        setRefusals(refusedMsgs);
        setStep('still_running');
      } else if (refusedMsgs.length > 0) {
        setRefusals(refusedMsgs);
        setStep('refused');
      } else {
        if (onStopped) onStopped();
        onClose();
      }
    } catch (err) {
      setError(err);
      setStep(force ? 'still_running' : 'detail');
    }
  }

  return html`
    <${Sheet} title=${title} onClose=${onClose}>
      ${step === 'confirm'
        ? html`
            <p>Stop ${program.name} on ${machine.name}? Anything unsaved in it is lost.</p>
            <div class="actions">
              <button type="button" class="btn" onClick=${() => setStep('detail')}>Cancel</button>
              <button
                type="button"
                class="btn danger full"
                onClick=${() => sendStop(verifiedTargets, false)}
              >
                Stop ${program.name}
              </button>
            </div>
          `
        : html`
            <div class="list">
              <dl class=${technical ? 'kv' : 'kv plain'}>
                <dt>${technical ? 'cpu' : 'Processor'}</dt>
                <dd>
                  ${technical
                    ? `${Math.round(program.wholeCpuPct)}% (${program.cpu_pct}% across cores)`
                    : `${Math.round(program.wholeCpuPct)}%`}
                </dd>
                <dt>${technical ? 'memory' : 'Memory'}</dt>
                <dd>
                  ${formatBytes(program.memory)}
                  ${program.memPct >= 1 ? ` (${Math.round(program.memPct)}%)` : ''}
                </dd>
                ${program.gpuMemBytes > 0
                  ? html`
                      <dt>${technical ? 'gpu_memory' : 'Graphics memory'}</dt>
                      <dd>${formatBytes(program.gpuMemBytes)}</dd>
                    `
                  : null}
                ${program.earliestStartTime
                  ? html`
                      <dt>${technical ? 'started' : 'Running since'}</dt>
                      <dd>${formatFriendlyRelativeTime(program.earliestStartTime)}</dd>
                    `
                  : null}
                ${program.user
                  ? html`
                      <dt>${technical ? 'user' : 'Running as'}</dt>
                      <dd>${program.user}</dd>
                    `
                  : null}
                ${technical && program.cmd
                  ? html`
                      <dt>command</dt>
                      <dd>${program.cmd}</dd>
                    `
                  : null}
                ${technical && pidList.length > 0
                  ? html`
                      <dt>pids</dt>
                      <dd>
                        ${pidList.join(', ')}
                        ${program.more > 0 ? ` (+${program.more} more)` : ''}
                      </dd>
                    `
                  : null}
              </dl>
            </div>

            ${error ? html`<${Notice} error=${error} />` : null}

            ${refusals.map(
              (msg, idx) => html`
                <div class="notice bad" key=${idx} role="status">
                  <strong>${msg}</strong>
                </div>
              `,
            )}

            ${step === 'still_running'
              ? html`
                  <div class="notice" role="status">
                    <strong>${program.name} is still running.</strong>
                    <span>It did not stop on its own. You can force it to stop now.</span>
                  </div>
                `
              : null}

            ${isSelf
              ? html`
                  <div class="notice neutral" role="status">
                    <strong>Legion cannot stop this program.</strong>
                    <span>Legion is running inside it.</span>
                  </div>
                  <div class="actions">
                    <button type="button" class="btn full" onClick=${onClose}>Close</button>
                  </div>
                `
              : !canKill
                ? html`
                    <div class="notice neutral" role="status">
                      <strong>Stopping programs is not allowed on this device.</strong>
                    </div>
                    <div class="actions">
                      <button type="button" class="btn full" onClick=${onClose}>Close</button>
                    </div>
                  `
                : step === 'still_running'
                  ? html`
                      <div class="actions">
                        <button type="button" class="btn" onClick=${onClose}>Close</button>
                        <button
                          type="button"
                          class="btn danger full"
                          onClick=${() => sendStop(stillTargets, true)}
                        >
                          Force stop
                        </button>
                      </div>
                    `
                  : step === 'refused'
                    ? html`
                        <div class="actions">
                          <button type="button" class="btn full" onClick=${onClose}>Close</button>
                        </div>
                      `
                    : html`
                        <div class="actions">
                          <button
                            type="button"
                            class="btn"
                            disabled=${step === 'stopping'}
                            onClick=${onClose}
                          >
                            Close
                          </button>
                          <button
                            type="button"
                            class="btn danger full"
                            disabled=${step === 'stopping' || (verifiedTargets.length === 0 && pidList.length === 0)}
                            onClick=${() => setStep('confirm')}
                          >
                            ${step === 'stopping' ? 'Stopping…' : 'Stop'}
                          </button>
                        </div>
                      `}
          `}
    <//>
  `;
}

// ── Overview tab ─────────────────────────────────────────────────────────────────────────────

function OverviewTab({
  machine,
  nodeInfo,
  link,
  capsStore = null,
  allowedSet,
  technical,
  wide,
  hideWideChrome = false,
  wakePending,
  onRegisterRefresh = null,
  onEnsureAwake,
  onStopJob,
  onRefreshFleet,
  onUpdateMachineSys,
}) {
  const [, setCapsRev] = useState(0);
  const [liveSys, setLiveSys] = useState(() => machine.sys || null);
  const [liveProcs, setLiveProcs] = useState(null);
  const [stale, setStale] = useState(false);
  const [selectedProgram, setSelectedProgram] = useState(null);
  const pollerRef = useRef(null);
  const on = targetFor(machine.name, nodeInfo?.node);

  useEffect(() => {
    if (!capsStore || !machine?.reachable || !machine?.name) return undefined;
    const unsub = capsStore.subscribe(() => setCapsRev((n) => n + 1));
    capsStore.ensure(machine.name).catch(() => {});
    return unsub;
  }, [capsStore, machine?.name, machine?.reachable]);

  useEffect(() => {
    setLiveSys(machine.sys || null);
  }, [machine.name, machine.sys]);

  useEffect(() => {
    setLiveProcs(null);
    setSelectedProgram(null);
    setStale(false);
    if (!machine.reachable || !link) return undefined;

    let mounted = true;
    const poller = createOverviewPoller({
      isVisible: () =>
        (typeof document === 'undefined' ? true : document.visibilityState === 'visible') &&
        !link.isAppSuspended?.(),
      onStaleChange(nextStale) {
        if (mounted) setStale(nextStale);
      },
      async fetchTick() {
        if (capsStore && !capsStore.isKnown(machine.name)) {
          await capsStore.ensure(machine.name).catch(() => null);
          if (!mounted) return false;
        }
        const supportsGroupProcs = capsStore
          ? capsStore.acceptsArg(machine.name, 'sys.procs', 'group')
          : true;

        const infoCall = capsStore
          ? capsStore.call('sys.info', {}, { on }).then((r) => r.value)
          : link.call('sys.info', {}, { on });
        const procsCall = supportsGroupProcs
          ? capsStore
            ? capsStore.call('sys.procs', { group: true, limit: 12 }, { on }).then((r) => r.value)
            : link.call('sys.procs', { group: true, limit: 12 }, { on })
          : Promise.resolve(null);

        const [sysRes, procsRes] = await Promise.allSettled([infoCall, procsCall]);
        if (!mounted) return false;
        let gotAny = false;
        if (sysRes.status === 'fulfilled' && sysRes.value) {
          gotAny = true;
          setLiveSys(sysRes.value);
          if (typeof onUpdateMachineSys === 'function') {
            onUpdateMachineSys(machine.name, sysRes.value);
          }
        }
        if (supportsGroupProcs && procsRes.status === 'fulfilled' && Array.isArray(procsRes.value?.procs)) {
          gotAny = true;
          setLiveProcs(procsRes.value.procs);
        }
        return gotAny;
      },
    });
    pollerRef.current = poller;
    poller.start({ immediate: true });

    const onVis = () => poller.onVisibilityChange();
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', onVis);
    }
    return () => {
      mounted = false;
      poller.stop();
      pollerRef.current = null;
      if (typeof document !== 'undefined') {
        document.removeEventListener('visibilitychange', onVis);
      }
    };
  }, [machine.name, machine.reachable, link, capsStore, on]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function' || wide) return undefined;
    return onRegisterRefresh(async () => {
      await Promise.all([
        pollerRef.current ? pollerRef.current.refreshNow() : Promise.resolve(),
        onRefreshFleet ? onRefreshFleet() : Promise.resolve(),
      ]);
    });
  }, [onRegisterRefresh, wide, onRefreshFleet, machine.name, machine.reachable]);

  if (machine.status === 'asking' || machine.asking) {
    return html`<div class="empty">Checking ${machine.name}…</div>`;
  }

  if (!machine.reachable) {
    const sub = formatMachineDetailSubtitle(machine, Date.now(), { wakePending });
    const canWake = !wakePending && machine.canWake && allowedSet?.has('power.wake');
    return html`
      <div>
        <div class="notice">
          <strong>${wakePending ? `${machine.name} is waking up.` : `${machine.name} is off.`}</strong>
          <span>
            ${wakePending
              ? sub
              : `${sub !== 'Off' ? `${sub}. ` : ''}${
                  canWake
                    ? 'You can switch it on from here.'
                    : machine.canWake
                      ? 'This device is not allowed to wake machines.'
                      : 'Wake-on-LAN is not set up for this machine.'
                }`}
          </span>
        </div>
        ${canWake
          ? html`
              <div class="actions">
                <button
                  type="button"
                  class="btn primary"
                  onClick=${() => {
                    if (onEnsureAwake) {
                      onEnsureAwake(machine, { kind: 'wake', immediate: true, forgettable: true });
                    }
                  }}
                >
                  Wake ${machine.name}
                </button>
              </div>
            `
          : null}
      </div>
    `;
  }

  const sys = liveSys || machine.sys;
  const meters = buildOverviewMeters(sys, { technical });
  const busyPrograms = selectBusyPrograms(liveProcs || [], sys);
  const runningJobs = Array.isArray(machine.jobs) ? machine.jobs : [];
  const canCancel = allowedSet ? allowedSet.has('job.cancel') : true;
  const canKill = allowedSet ? allowedSet.has('sys.kill') : true;
  const activeBusy = isOverviewBusy(sys);
  const stateLine = formatOverviewStateLine(sys);

  const busyReasonLines = formatBusyReasonLines(machine.busyReasons);
  const busySummary = machine.busy
    ? busyReasonLines.length > 0
      ? null
      : machine.runningJobs > 0
        ? `${machine.runningJobs} ${machine.runningJobs === 1 ? 'thing' : 'things'} running`
        : 'Busy'
    : machine.busyState === 'unmonitored'
      ? 'Not monitored'
      : 'Idle';

  const systemLine = [
    sys?.os?.family ? osLabel(sys.os.family) : null,
    technical ? sys?.os?.kernel || sys?.os?.version : sys?.os?.version || null,
    technical && sys?.os?.arch ? `· ${sys.os.arch}` : null,
  ]
    .filter(Boolean)
    .join(' ')
    .replace(' ·', ' ·');

  const capsEntry = capsStore && machine?.name ? capsStore.get(machine.name) : null;
  const missingFeatures = capsStore && machine?.name ? capsStore.missingFeatures(machine.name) : [];
  const capsKnown = capsStore ? capsStore.isKnown(machine.name) : true;
  const canGroupProcs = capsStore ? capsKnown && capsStore.acceptsArg(machine.name, 'sys.procs', 'group') : true;
  const canKillTargets = capsStore ? capsKnown && capsStore.acceptsArg(machine.name, 'sys.kill', 'targets') : true;

  const hasAboutRows =
    Boolean(systemLine) ||
    busyReasonLines.length > 0 ||
    (technical && (sys?.uptime_s != null || Boolean(machine.version) || Boolean(busySummary)));

  return html`
    <div>
      ${wide && !hideWideChrome
        ? html`
            <div class=${stale ? 'machine-state stale-values' : 'machine-state'}>
              <span class=${activeBusy ? 'dot busy' : 'dot'} aria-hidden="true"></span>
              <span>${stateLine}</span>
            </div>
          `
        : null}

      ${missingFeatures.length > 0
        ? html`
            <${OlderMachineNote}
              machine=${machine.name}
              version=${capsEntry?.version || machine.version || null}
              build=${capsEntry?.build || machine.build || null}
              missing=${missingFeatures}
              technical=${technical}
            />
          `
        : null}

      ${meters.length > 0
        ? html`
            ${!(wide && hideWideChrome && missingFeatures.length === 0)
              ? html`<p class="label">Health</p>`
              : null}
            <div class=${stale ? 'list stale-values' : 'list'}>
              ${meters.map(
                (m) => html`
                  <div class=${m.amber ? 'gauge warn' : 'gauge'} key=${m.id}>
                    <header>
                      <span>
                        ${m.label}
                        ${m.subLabel ? html` <span class="mono dim">${m.subLabel}</span>` : null}
                      </span>
                      <span>${m.detail}</span>
                    </header>
                    <${Meter} value=${m.pct} level=${m.amber ? 'warn' : ''} />
                  </div>
                `,
              )}
            </div>
          `
        : null}

      ${canGroupProcs
        ? html`
            <p class="label">Using it now</p>
            <div class=${stale ? 'list stale-values' : 'list'}>
              ${liveProcs === null
                ? html`
                    <div class="row off">
                      <span class="body"><span class="sub">Checking…</span></span>
                    </div>
                  `
                : busyPrograms.length === 0
                  ? html`
                      <div class="row off">
                        <span class="body"><span class="sub">Nothing much is running.</span></span>
                      </div>
                    `
                  : busyPrograms.map(
                      (prog) => html`
                        <button
                          type="button"
                          class="row"
                          key=${`${prog.name}:${prog.user || ''}`}
                          onClick=${() => setSelectedProgram(prog)}
                        >
                          <span class="body">
                            <span class="title">
                              ${prog.name}
                              ${prog.count > 1 ? html`<span class="proc-count">× ${prog.count}</span>` : null}
                            </span>
                            <span class="sub">${prog.primaryText}</span>
                          </span>
                          <span class="chev" aria-hidden="true">›</span>
                        </button>
                      `,
                    )}
            </div>
          `
        : null}
      ${runningJobs.length > 0
        ? html`
            <p class="label">Running now</p>
            <div class="list">
              ${runningJobs.map(
                (job) => html`
                  <${ActivityRow}
                    key=${job.id}
                    job=${{ ...job, machine: machine.name }}
                    href=${routeHref({
                      section: 'machines',
                      machine: machine.name,
                      tab: 'activity',
                      jobId: job.id,
                    })}
                    includeMachine=${false}
                    technical=${technical}
                    canCancel=${canCancel}
                    onStop=${onStopJob}
                  />
                `,
              )}
            </div>
          `
        : null}

      ${hasAboutRows
        ? html`
            <p class="label">About</p>
            <div class="list">
              <dl class=${technical ? 'kv' : 'kv plain'}>
                ${systemLine
                  ? html`
                      <dt>${technical ? 'system' : 'System'}</dt>
                      <dd>${systemLine}</dd>
                    `
                  : null}
                ${technical && sys?.uptime_s != null
                  ? html`
                      <dt>up</dt>
                      <dd>${formatFriendlyUptime(sys.uptime_s)}</dd>
                    `
                  : null}
                ${technical && machine.version
                  ? html`
                      <dt>legion</dt>
                      <dd>${machine.version}</dd>
                    `
                  : null}
                ${busyReasonLines.length > 0
                  ? html`
                      <dt>${technical ? 'busy' : 'Busy'}</dt>
                      <dd class="allows-lines">
                        ${busyReasonLines.map((line, i) => html`<span key=${i}>${line}</span>`)}
                      </dd>
                    `
                  : technical
                    ? html`
                        <dt>busy</dt>
                        <dd>${busySummary}</dd>
                      `
                    : null}
              </dl>
            </div>
          `
        : null}

      ${selectedProgram
        ? html`
            <${ProgramSheet}
              program=${selectedProgram}
              machine=${machine}
              on=${on}
              link=${link}
              capsStore=${capsStore}
              canKill=${canKill && canKillTargets}
              technical=${technical}
              onStopped=${() => pollerRef.current?.refreshNow()}
              onClose=${() => setSelectedProgram(null)}
            />
          `
        : null}
    </div>
  `;
}

// ── Command composer (docked at bottom on phone; bottom of pane on wide) ─────────────────────

export function CommandComposer({
  machine,
  nodeInfo,
  link,
  allowedSet,
  state,
  onStateChange,
  onAppendRun,
  onUpdateRun,
  onOpenJob,
  onEnsureAwake,
}) {
  const abortRef = useRef(null);
  const canExec = allowedSet ? allowedSet.has('exec.run') : true;
  const canJob = allowedSet ? allowedSet.has('job.start') : true;
  const on = targetFor(machine.name, nodeInfo?.node);

  useEffect(() => {
    return () => {
      if (abortRef.current) abortRef.current.abort();
    };
  }, [machine.name]);

  if (!canExec && !canJob) return null;

  const cmd = state.cmd || '';
  const asJob = Boolean(state.asJob);
  const jobName = state.jobName || '';
  const running = Boolean(state.running);

  async function executeSubmitted(trimmed, runAsJob, nameForJob) {
    const runId = `run-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;

    if (runAsJob && canJob) {
      onStateChange({ running: true, error: null });
      try {
        const args = { cmd: trimmed };
        if (nameForJob.trim()) args.name = nameForJob.trim();
        const res = await link.call('job.start', args, { on });
        onAppendRun(machine.name, {
          id: runId,
          cmd: trimmed,
          jobId: res.id,
          jobName: res.name || nameForJob.trim() || '',
          status: 'started',
          at: Date.now(),
        });
        onStateChange({ cmd: '', jobName: '', running: false, error: null });
      } catch (err) {
        onStateChange({ running: false, error: err });
      }
      return;
    }

    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;

    onAppendRun(machine.name, {
      id: runId,
      cmd: trimmed,
      status: 'running',
      chunks: [],
      exitCode: null,
      durationMs: null,
      at: Date.now(),
    });
    onStateChange({ cmd: '', running: true, error: null });

    let streamed = false;
    try {
      const res = await link.call(
        'exec.run',
        { cmd: trimmed },
        {
          on,
          signal: ctrl.signal,
          onEvent(evt) {
            if (!evt) return;
            if (evt.kind === 'stdout' || evt.kind === 'stderr') {
              streamed = true;
              onUpdateRun(machine.name, runId, (prev) => ({
                ...prev,
                chunks: [...(prev.chunks || []), { kind: evt.kind, text: String(evt.data ?? '') }],
              }));
            }
          },
        },
      );
      onUpdateRun(machine.name, runId, (prev) => {
        const chunks = streamed
          ? prev.chunks || []
          : [
              ...(res.stdout ? [{ kind: 'stdout', text: res.stdout }] : []),
              ...(res.stderr ? [{ kind: 'stderr', text: res.stderr }] : []),
            ];
        return {
          ...prev,
          chunks,
          status: res.timed_out ? 'timed_out' : res.exit_code === 0 ? 'succeeded' : 'failed',
          exitCode: res.exit_code ?? null,
          durationMs: res.duration_ms ?? 0,
        };
      });
    } catch (err) {
      if (err?.code === 'cancelled') {
        onUpdateRun(machine.name, runId, (prev) => ({ ...prev, status: 'cancelled' }));
      } else {
        onUpdateRun(machine.name, runId, (prev) => ({ ...prev, status: 'error', error: err }));
      }
    } finally {
      abortRef.current = null;
      onStateChange({ running: false });
    }
  }

  async function submit(e) {
    e.preventDefault();
    if (running && abortRef.current) {
      abortRef.current.abort();
      return;
    }
    const trimmed = cmd.trim();
    if (!trimmed) return;

    if (!machine.reachable && onEnsureAwake) {
      const shortCmd = trimmed.length > 48 ? `${trimmed.slice(0, 47)}…` : trimmed;
      const res = await onEnsureAwake(machine, {
        kind: 'command',
        label: `run ${shortCmd}`,
        onContinue: ({ inBackground }) =>
          executeSubmitted(trimmed, inBackground && canJob ? true : asJob, jobName),
      });
      if (!res?.awake && res?.canWake === false) {
        onStateChange({
          error: {
            code: 'unreachable',
            message: `${machine.name} is off.`,
            hint: machine.canWake
              ? 'This device is not allowed to wake machines.'
              : 'Wake-on-LAN is not set up for this machine.',
          },
        });
      }
      return;
    }

    await executeSubmitted(trimmed, asJob, jobName);
  }

  return html`
    <div>
      ${state.error ? html`<${Notice} error=${state.error} />` : null}
      <form class="prompt" onSubmit=${submit}>
        <input
          type="text"
          value=${cmd}
          onInput=${(e) => onStateChange({ cmd: e.target.value })}
          placeholder="df -h /"
          aria-label="Command"
          autocomplete="off"
          spellcheck="false"
        />
        <button
          type="submit"
          class=${running ? 'btn danger' : 'btn primary'}
          disabled=${!running && !cmd.trim()}
        >
          ${running ? 'Stop' : 'Run'}
        </button>
      </form>
      ${canJob
        ? html`
            <label class="switch dock-switch">
              <span>Keep running after I leave</span>
              <input
                type="checkbox"
                checked=${asJob}
                onChange=${(e) => onStateChange({ asJob: e.target.checked })}
              />
            </label>
          `
        : null}
      ${canJob && asJob
        ? html`
            <div class="dock-name">
              <input
                class="input plain"
                type="text"
                value=${jobName}
                onInput=${(e) => onStateChange({ jobName: e.target.value })}
                placeholder="Name it (optional, e.g. Photo backup)"
                aria-label="Name for background task"
                autocomplete="off"
              />
            </div>
          `
        : null}
    </div>
  `;
}

function CommandStream({ machine, runs, technical, allowedSet, nodeInfo, onOpenJob }) {
  const endRef = useRef(null);
  const canExec = allowedSet ? allowedSet.has('exec.run') : true;
  const canJob = allowedSet ? allowedSet.has('job.start') : true;

  useEffect(() => {
    if (endRef.current && typeof endRef.current.scrollIntoView === 'function') {
      endRef.current.scrollIntoView({ block: 'end', behavior: 'auto' });
    }
  }, [runs?.length, runs?.[runs.length - 1]?.chunks?.length]);

  if (!canExec && !canJob) {
    return html`
      <${Notice}
        title="Running commands is not allowed."
        hint=${technical
          ? unavailableReason({ name: 'exec.run', effect: 'exec' }, nodeInfo?.whoami)
          : plainDenyReason('exec.run', nodeInfo?.whoami)}
      />
    `;
  }

  if (!machine.reachable) {
    return html`<div class="empty">${machine.name} is off.</div>`;
  }

  if (!runs || runs.length === 0) {
    return html`
      <div class="empty">
        Run a command on ${machine.name}. Output appears here above the input.
      </div>
    `;
  }

  return html`
    ${runs.map((entry) => {
      if (entry.jobId) {
        const displayTitle = technical
          ? entry.jobName || entry.cmd || entry.jobId
          : friendlyJobName({ name: entry.jobName, command: entry.cmd });
        return html`
          <div class="notice" key=${entry.id}>
            <strong>Started ${displayTitle}</strong>
            <span>Running in the background on ${machine.name}.</span>
            <div class="row-actions">
              <button
                type="button"
                class="btn small primary"
                onClick=${() => onOpenJob(entry.jobId)}
              >
                Follow output
              </button>
            </div>
          </div>
        `;
      }
      if (entry.error) {
        return html`<${Notice} key=${entry.id} error=${entry.error} />`;
      }
      const meta = formatCommandMeta(entry, technical);
      const chunks = entry.chunks || [];
      return html`
        <pre class="out" key=${entry.id}>
<span class="meta">$ ${entry.cmd}${meta ? ` · ${meta}` : ''}${'\n'}</span>${chunks.length > 0
            ? chunks.map((c, i) =>
                c.kind === 'stderr'
                  ? html`<span key=${i} class="err">${c.text}</span>`
                  : html`<span key=${i}>${c.text}</span>`,
              )
            : entry.status === 'running'
              ? html`<span class="meta">Running…</span>`
              : null}</pre>
      `;
    })}
    <div ref=${endRef} aria-hidden="true"></div>
  `;
}

// ── More tab (phone 4th bottom part) ─────────────────────────────────────────────────────────

function MoreTab({ machine }) {
  const items = [
    {
      tab: 'command',
      title: 'Command',
      sub: 'Run a command and see what it says',
    },
    {
      tab: 'services',
      title: 'Services',
      sub: 'Check, start or restart background services',
    },
    {
      tab: 'projects',
      title: 'Projects',
      sub: 'Pull code and run tasks',
    },
    {
      tab: 'activity',
      title: `Activity on ${machine.name}`,
      sub: 'Running and recent work on this machine',
    },
  ];

  return html`
    <div class="list">
      ${items.map(
        (item) => html`
          <a
            key=${item.tab}
            class="row"
            href=${routeHref({ section: 'machines', machine: machine.name, tab: item.tab })}
          >
            <span class="body">
              <span class="title">${item.title}</span>
              <span class="sub">${item.sub}</span>
            </span>
            <span class="chev" aria-hidden="true">›</span>
          </a>
        `,
      )}
    </div>
  `;
}

// ── Machine Activity tab ─────────────────────────────────────────────────────────────────────

function MachineActivityTab({
  machine,
  nodeInfo,
  link,
  allowedSet,
  technical,
  jobId,
  onObserveTransferJobs = null,
  onRegisterRefresh = null,
  onRefreshFleet,
}) {
  const [jobs, setJobs] = useState(null);
  const [error, setError] = useState(null);
  const on = targetFor(machine.name, nodeInfo?.node);
  const canCancel = allowedSet ? allowedSet.has('job.cancel') : true;

  async function load() {
    if (!machine.reachable) {
      setJobs([]);
      return;
    }
    setError(null);
    try {
      const res = await link.call('job.list', { limit: 50 }, { on });
      const list = Array.isArray(res?.jobs)
        ? res.jobs.map((j) => ({ ...j, machine: machine.name }))
        : [];
      setJobs(list);
      if (onObserveTransferJobs && list.length > 0) {
        onObserveTransferJobs(list);
      }
    } catch (err) {
      setError(err);
    }
  }

  useEffect(() => {
    load();
  }, [machine.name, machine.reachable, jobId]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function' || jobId) return undefined;
    return onRegisterRefresh(() => load());
  }, [onRegisterRefresh, jobId, machine.name, machine.reachable]);

  async function handleStop(job) {
    setError(null);
    try {
      await link.call('job.cancel', { id: job.id }, { on });
      await load();
      if (onRefreshFleet) onRefreshFleet();
    } catch (err) {
      setError(err);
    }
  }

  if (jobId) {
    return html`
      <${ActivityDetail}
        machineName=${machine.name}
        nodeName=${nodeInfo?.node}
        jobId=${jobId}
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

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}
      <${ActivityGroups}
        jobs=${jobs}
        includeMachine=${false}
        technical=${technical}
        canCancel=${canCancel}
        hrefForJob=${(job) =>
          routeHref({ section: 'machines', machine: machine.name, tab: 'activity', jobId: job.id })}
        onStopJob=${handleStop}
        emptyText=${`Nothing has run on ${machine.name} yet.`}
      />
    </div>
  `;
}

// ── Services tab ─────────────────────────────────────────────────────────────────────────────

function ServicesTab({ machine, nodeInfo, link, allowedSet, onRegisterRefresh = null }) {
  const [services, setServices] = useState(null);
  const [filter, setFilter] = useState('');
  const [selected, setSelected] = useState(null);
  const [logs, setLogs] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const on = targetFor(machine.name, nodeInfo?.node);
  const filterRef = useRef(filter);
  filterRef.current = filter;
  const selectedRef = useRef(selected);
  selectedRef.current = selected;

  async function load(q = filter) {
    if (!machine.reachable) {
      setServices([]);
      return;
    }
    setError(null);
    try {
      const args = { limit: 100 };
      if (q.trim()) args.filter = q.trim();
      const res = await link.call('svc.list', args, { on });
      setServices(Array.isArray(res?.services) ? res.services : []);
    } catch (err) {
      setError(err);
    }
  }

  useEffect(() => {
    setSelected(null);
    setLogs(null);
    load('');
  }, [machine.name, machine.reachable]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(async () => {
      if (selectedRef.current) {
        await openService(selectedRef.current);
      } else {
        await load(filterRef.current);
      }
    });
  }, [onRegisterRefresh, machine.name, machine.reachable]);

  async function openService(svc) {
    setSelected(svc);
    setLogs(null);
    setError(null);
    try {
      const res = await link.call('svc.logs', { name: svc.name, scope: svc.scope, lines: 50 }, { on });
      setLogs(res);
    } catch (err) {
      setError(err);
    }
  }

  async function act(tool, svc) {
    setBusy(true);
    setError(null);
    try {
      const updated = await link.call(tool, { name: svc.name, scope: svc.scope }, { on });
      setSelected((prev) => (prev ? { ...prev, state: updated.state || prev.state } : prev));
      await load();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  if (!machine.reachable) {
    return html`<div class="empty">${machine.name} is off.</div>`;
  }

  return html`
    <div>
      <form
        class="search-row"
        onSubmit=${(e) => {
          e.preventDefault();
          load(filter);
        }}
      >
        <input
          class="input plain"
          type="search"
          value=${filter}
          onInput=${(e) => {
            setFilter(e.target.value);
            load(e.target.value);
          }}
          placeholder="Filter services"
          aria-label="Filter services"
        />
      </form>

      ${error ? html`<${Notice} error=${error} />` : null}

      ${selected
        ? html`
            <div class="result-head">
              <p class="label">${selected.name} · ${selected.scope}</p>
              <button type="button" class="btn small" onClick=${() => setSelected(null)}>
                All services
              </button>
            </div>
            <div class="actions">
              ${allowedSet?.has('svc.start')
                ? html`
                    <button
                      type="button"
                      class="btn small"
                      disabled=${busy}
                      onClick=${() => act('svc.start', selected)}
                    >
                      Start
                    </button>
                  `
                : null}
              ${allowedSet?.has('svc.restart')
                ? html`
                    <button
                      type="button"
                      class="btn small"
                      disabled=${busy}
                      onClick=${() => act('svc.restart', selected)}
                    >
                      Restart
                    </button>
                  `
                : null}
              ${allowedSet?.has('svc.stop')
                ? html`
                    <button
                      type="button"
                      class="btn small"
                      disabled=${busy}
                      onClick=${() => act('svc.stop', selected)}
                    >
                      Stop
                    </button>
                  `
                : null}
            </div>
            <p class="label">Recent logs</p>
            <pre class="out wrap">
${logs === null
              ? 'Loading logs…'
              : logs.lines?.length
                ? logs.lines.join('\n')
                : 'No recent log lines.'}</pre>
          `
        : services === null
          ? html`<div class="empty">Loading services…</div>`
          : services.length === 0
            ? html`
                <div class="empty">
                  ${filter.trim() ? `No services match "${filter.trim()}".` : `No services found on ${machine.name}.`}
                </div>
              `
            : html`
                <div class="list spaced">
                  ${services.map(
                    (svc) => html`
                      <button
                        type="button"
                        key=${`${svc.scope}:${svc.name}`}
                        class="row"
                        onClick=${() => openService(svc)}
                      >
                        <span
                          class=${svc.state === 'running'
                            ? 'dot'
                            : svc.state === 'failed'
                              ? 'dot busy'
                              : 'dot off'}
                          aria-hidden="true"
                        ></span>
                        <span class="body">
                          <span class="title mono">${svc.name}</span>
                          <span class="sub">
                            ${svc.scope} · ${svc.state}
                            ${svc.description ? ` · ${svc.description}` : ''}
                          </span>
                        </span>
                      </button>
                    `,
                  )}
                </div>
              `}
    </div>
  `;
}

// ── Projects tab ─────────────────────────────────────────────────────────────────────────────

function ProjectsTab({ machine, nodeInfo, link, allowedSet, onRegisterRefresh = null }) {
  const [projects, setProjects] = useState(null);
  const [output, setOutput] = useState('');
  const [busyKey, setBusyKey] = useState('');
  const [error, setError] = useState(null);
  const on = targetFor(machine.name, nodeInfo?.node);

  async function load() {
    if (!machine.reachable) {
      setProjects([]);
      return;
    }
    setError(null);
    try {
      const [listed, statuses] = await Promise.all([
        link.call('proj.list', {}, { on }),
        link.call('proj.status', {}, { on }).catch(() => ({ projects: [] })),
      ]);
      const byName = new Map((statuses?.projects || []).map((p) => [p.name, p]));
      const merged = (listed?.projects || []).map((p) => ({
        ...p,
        status: byName.get(p.name) || null,
      }));
      setProjects(merged);
    } catch (err) {
      setError(err);
    }
  }

  useEffect(() => {
    setOutput('');
    load();
  }, [machine.name, machine.reachable]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(() => load());
  }, [onRegisterRefresh, machine.name, machine.reachable]);

  async function pull(project) {
    setBusyKey(`pull:${project.name}`);
    setError(null);
    setOutput('');
    try {
      const res = await link.call('proj.pull', { project: project.name }, { on });
      setOutput(res.summary || `Updated ${project.name}.`);
      await load();
    } catch (err) {
      setError(err);
    } finally {
      setBusyKey('');
    }
  }

  async function clone(project) {
    setBusyKey(`clone:${project.name}`);
    setError(null);
    setOutput('');
    try {
      const res = await link.call('proj.clone', { project: project.name }, { on });
      setOutput(`Cloned ${project.name} at ${res.commit || 'HEAD'}.`);
      await load();
    } catch (err) {
      setError(err);
    } finally {
      setBusyKey('');
    }
  }

  async function runTask(project, task) {
    setBusyKey(`run:${project.name}:${task}`);
    setError(null);
    setOutput('');
    try {
      const res = await link.call(
        'proj.run',
        { project: project.name, task },
        {
          on,
          onEvent(evt) {
            if (evt?.kind === 'stdout' || evt?.kind === 'stderr') {
              setOutput((prev) => prev + String(evt.data ?? ''));
            }
          },
        },
      );
      if (res?.stdout) setOutput(res.stdout + (res.stderr || ''));
      else if (res?.id) setOutput(`Started background task ${res.id}`);
    } catch (err) {
      setError(err);
    } finally {
      setBusyKey('');
    }
  }

  if (!machine.reachable) {
    return html`<div class="empty">${machine.name} is off.</div>`;
  }

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${projects === null
        ? html`<div class="empty">Loading projects…</div>`
        : projects.length === 0
          ? html`<div class="empty">No projects recorded on ${machine.name}.</div>`
          : html`
              <div class="list">
                ${projects.map((p) => {
                  const git = p.status?.git;
                  const subParts = [];
                  if (p.path) subParts.push(p.path);
                  if (git?.branch) subParts.push(git.branch);
                  if (git && (git.changed > 0 || git.staged > 0)) {
                    subParts.push(`${git.changed + git.staged} changed`);
                  }
                  if (!p.present) subParts.push('not cloned here');
                  return html`
                    <div class="row" key=${p.name}>
                      <div class="body">
                        <div class="title mono">${p.name}</div>
                        <div class="sub wrap">${subParts.join(' · ') || p.description || ''}</div>
                        <div class="row-actions">
                          ${p.present && allowedSet?.has('proj.pull')
                            ? html`
                                <button
                                  type="button"
                                  class="btn small"
                                  disabled=${Boolean(busyKey)}
                                  onClick=${() => pull(p)}
                                >
                                  ${busyKey === `pull:${p.name}` ? 'Pulling…' : 'Pull'}
                                </button>
                              `
                            : null}
                          ${!p.present && p.repo && allowedSet?.has('proj.clone')
                            ? html`
                                <button
                                  type="button"
                                  class="btn small"
                                  disabled=${Boolean(busyKey)}
                                  onClick=${() => clone(p)}
                                >
                                  ${busyKey === `clone:${p.name}` ? 'Cloning…' : 'Clone'}
                                </button>
                              `
                            : null}
                          ${p.present && allowedSet?.has('proj.run')
                            ? (p.tasks || []).map(
                                (task) => html`
                                  <button
                                    type="button"
                                    key=${task}
                                    class="btn small"
                                    disabled=${Boolean(busyKey)}
                                    onClick=${() => runTask(p, task)}
                                  >
                                    ${busyKey === `run:${p.name}:${task}` ? `${task}…` : `Run ${task}`}
                                  </button>
                                `,
                              )
                            : null}
                        </div>
                      </div>
                    </div>
                  `;
                })}
              </div>
            `}
      ${output ? html`<pre class="out wrap">${output}</pre>` : null}
    </div>
  `;
}

// ── Power tab ────────────────────────────────────────────────────────────────────────────────

function PowerTab({
  machine,
  nodeInfo,
  link,
  allowedSet,
  technical,
  wakePending,
  onRegisterRefresh = null,
  onEnsureAwake,
  onWakeSent,
  onRequestConfirm,
  onRefreshFleet,
}) {
  const [status, setStatus] = useState(null);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  const on = targetFor(machine.name, nodeInfo?.node);

  async function load() {
    if (!machine.reachable) {
      setStatus(null);
      return;
    }
    setError(null);
    try {
      const res = await link.call('power.status', {}, { on });
      setStatus(res);
    } catch (err) {
      setError(err);
    }
  }

  useEffect(() => {
    setResult(null);
    load();
  }, [machine.name, machine.reachable]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(async () => {
      await Promise.all([load(), onRefreshFleet ? onRefreshFleet() : Promise.resolve()]);
    });
  }, [onRegisterRefresh, onRefreshFleet, machine.name, machine.reachable]);

  async function runAction(tool, args, target, hasDry) {
    setError(null);
    setResult(null);
    const liveJobs = Array.isArray(machine.jobs) ? machine.jobs : [];
    const busyReasons =
      status?.busy?.state === 'busy' ? status.busy.reasons || [] : machine.busyReasons || [];
    const copy = describePowerConfirm(tool, machine.name, args, liveJobs, busyReasons);
    onRequestConfirm({
      title: copy.title,
      summary: copy.summary,
      jobs: tool === 'power.wake' ? [] : liveJobs,
      target: target || nodeInfo?.node,
      targetName: machine.name,
      args,
      tool,
      effect: 'power',
      confirmLabel: copy.confirmLabel,
      canDryRun: hasDry,
      async onDryRun() {
        const dry = await link.call(tool, { ...args, dry_run: true }, { on: target });
        setResult(dry);
        return dry;
      },
      async onConfirm() {
        setBusy(true);
        try {
          const out = await link.call(tool, { ...args, dry_run: false }, { on: target });
          setResult(out);
          if (tool === 'power.wake' && onWakeSent) {
            onWakeSent(args?.machine || machine.name);
          }
          await load();
          if (onRefreshFleet) onRefreshFleet();
        } catch (err) {
          setError(err);
        } finally {
          setBusy(false);
        }
      },
    });
  }

  if (machine.status === 'asking' || machine.asking) {
    return html`<div class="empty">Checking ${machine.name}…</div>`;
  }

  if (!machine.reachable) {
    const canWake = allowedSet?.has('power.wake');
    const sub = formatMachineDetailSubtitle(machine, Date.now(), { wakePending });
    return html`
      <div>
        <div class="notice">
          <strong>${wakePending ? `${machine.name} is waking up.` : `${machine.name} is off.`}</strong>
          <span>
            ${wakePending
              ? sub
              : `${sub !== 'Off' ? `${sub}. ` : ''}${
                  machine.canWake
                    ? canWake
                      ? 'Send a wake signal to turn it on.'
                      : technical
                        ? unavailableReason({ name: 'power.wake', effect: 'power' }, nodeInfo?.whoami)
                        : plainDenyReason('power.wake', nodeInfo?.whoami)
                    : 'Wake-on-LAN is not configured for this machine.'
                }`}
          </span>
        </div>
        ${machine.canWake && !wakePending
          ? html`
              <div class="list">
                <div class=${canWake ? 'row' : 'row off'}>
                  <div class="body">
                    <div class="title">Wake ${machine.name}</div>
                    <div class="sub wrap">Switch it on over the network</div>
                  </div>
                  <button
                    type="button"
                    class="btn small primary"
                    disabled=${!canWake || busy}
                    onClick=${() => {
                      if (onEnsureAwake) {
                        onEnsureAwake(machine, { kind: 'wake', immediate: true, forgettable: true });
                      } else {
                        runAction('power.wake', { machine: machine.name }, undefined, true);
                      }
                    }}
                  >
                    Wake
                  </button>
                </div>
              </div>
            `
          : null}
        ${error ? html`<${Notice} error=${error} />` : null}
        ${result && technical ? html`<pre class="out wrap">${JSON.stringify(result, null, 2)}</pre>` : null}
      </div>
    `;
  }

  const actions = [
    {
      tool: 'power.sleep',
      label: `Sleep ${machine.name}`,
      verb: 'Sleep',
      desc: `Put ${machine.name} to sleep after a few seconds.`,
      args: {},
      dry: true,
    },
    {
      tool: 'power.restart',
      label: `Restart ${machine.name}`,
      verb: 'Restart',
      desc: `Restart ${machine.name} after a short delay.`,
      args: {},
      dry: true,
    },
    ...(status?.targets || []).map((t) => ({
      tool: 'power.restart',
      label: `Restart into ${t.name}`,
      verb: `Restart into ${t.name}`,
      desc: technical
        ? `Boot into "${t.name}" (${t.how}) once and restart ${machine.name}.`
        : `Start ${machine.name} into ${t.name} once.`,
      args: { into: t.name },
      dry: true,
    })),
    {
      tool: 'power.shutdown',
      label: `Shut down ${machine.name}`,
      verb: 'Shut down',
      desc: `Switch ${machine.name} off after a short delay.`,
      args: {},
      dry: true,
    },
  ];

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}

      ${status?.pending
        ? html`
            <div class="notice">
              <strong>Pending ${status.pending.action} on ${machine.name}</strong>
              <span>Scheduled to run soon.</span>
              ${allowedSet?.has('power.cancel')
                ? html`
                    <div class="row-actions">
                      <button
                        type="button"
                        class="btn small danger"
                        disabled=${busy}
                        onClick=${async () => {
                          setBusy(true);
                          try {
                            const out = await link.call('power.cancel', {}, { on });
                            setResult(out);
                            await load();
                          } catch (err) {
                            setError(err);
                          } finally {
                            setBusy(false);
                          }
                        }}
                      >
                        Cancel pending action
                      </button>
                    </div>
                  `
                : null}
            </div>
          `
        : null}

      ${status
        ? (() => {
            const powerReasons = formatBusyReasonLines(status.busy?.reasons);
            const stateLabel =
              status.busy?.state === 'unmonitored'
                ? 'Not monitored'
                : status.busy?.state === 'busy'
                  ? 'Busy'
                  : status.busy?.state === 'idle'
                    ? 'Idle'
                    : status.busy?.state || 'Unknown';
            return html`
              <div class="list">
                <dl class=${technical ? 'kv' : 'kv plain'}>
                  <dt>${technical ? 'state' : 'Status'}</dt>
                  <dd>${stateLabel}</dd>
                  ${powerReasons.length > 0
                    ? html`
                        <dt>${technical ? 'reasons' : 'Why'}</dt>
                        <dd class="allows-lines">
                          ${powerReasons.map((line, i) => html`<span key=${i}>${line}</span>`)}
                        </dd>
                      `
                    : null}
                  ${technical && status.system
                    ? html`
                        <dt>system</dt>
                        <dd>${status.system}</dd>
                      `
                    : null}
                  ${technical
                    ? html`
                        <dt>sudo</dt>
                        <dd>${status.sudo ? 'yes' : 'no'}</dd>
                      `
                    : null}
                </dl>
              </div>
            `;
          })()
        : !error
          ? html`<div class="empty">Loading power state…</div>`
          : null}

      <p class="label">Power</p>
      <div class="list">
        ${actions.map((item) => {
          const allowed = allowedSet ? allowedSet.has(item.tool) : true;
          return html`
            <div class=${allowed ? 'row' : 'row off'} key=${`${item.tool}:${item.label}`}>
              <div class="body">
                <div class="title">
                  <span>${item.label}</span>
                  ${technical ? html`<span class="chip fx power">power</span>` : null}
                </div>
                <div class="sub wrap">
                  ${allowed
                    ? item.desc
                    : technical
                      ? unavailableReason({ name: item.tool, effect: 'power' }, nodeInfo?.whoami)
                      : plainDenyReason(item.tool, nodeInfo?.whoami)}
                </div>
              </div>
              <button
                type="button"
                class="btn small"
                disabled=${!allowed || busy}
                onClick=${() => runAction(item.tool, item.args, on, item.dry)}
              >
                ${allowed ? item.verb : 'Unavailable'}
              </button>
            </div>
          `;
        })}
      </div>

      ${result && technical ? html`<pre class="out wrap">${JSON.stringify(result, null, 2)}</pre>` : null}
    </div>
  `;
}

// ── MachineDetail shell ──────────────────────────────────────────────────────────────────────

export function MachineDetail({
  machine,
  tab,
  jobId,
  wide,
  hideWideChrome = false,
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
  wakePending,
  onEnsureAwake,
  onWakeSent,
  onRequestConfirm,
  onRefreshFleet,
  onUpdateMachineSys,
  commandRuns,
  onCommandRunsChange,
}) {
  const technical = Boolean(settings?.technical);
  const effectiveTab = wide && tab === 'more' ? 'overview' : tab || 'overview';
  const dotClass =
    machine.status === 'asking' || machine.asking
      ? 'dot asking'
      : !machine.reachable
        ? 'dot off'
        : machine.busy
          ? 'dot busy'
          : 'dot';

  function selectTab(nextTab) {
    window.location.hash = routeHref({ section: 'machines', machine: machine.name, tab: nextTab });
  }

  function openJob(id) {
    window.location.hash = routeHref({
      section: 'machines',
      machine: machine.name,
      tab: 'activity',
      jobId: id,
    });
  }

  async function handleStopJob(job) {
    const on = targetFor(machine.name, nodeInfo?.node);
    await link.call('job.cancel', { id: job.id }, { on });
    if (onRefreshFleet) onRefreshFleet();
  }

  const runs = commandRuns?.byMachine?.[machine.name] || [];

  // On phone, CommandTab renders directly into `.main.command-stream` while `app.js` renders the
  // docked composer in `.dock`.
  if (!wide && effectiveTab === 'command') {
    return html`
      <${CommandStream}
        machine=${machine}
        runs=${runs}
        technical=${technical}
        allowedSet=${allowedSet}
        nodeInfo=${nodeInfo}
        onOpenJob=${openJob}
      />
    `;
  }

  return html`
    <div>
      ${wide && !hideWideChrome
        ? html`
            <h2 class="pane-title">
              <span class=${dotClass} aria-hidden="true"></span>
              ${machine.name}
            </h2>
            <div class="tabs" role="tablist" aria-label=${`${machine.name} sections`}>
              ${WIDE_MACHINE_TABS.map(
                (id) => html`
                  <button
                    type="button"
                    key=${id}
                    role="tab"
                    aria-selected=${effectiveTab === id ? 'true' : 'false'}
                    onClick=${() => selectTab(id)}
                  >
                    ${WIDE_TAB_LABELS[id]}
                  </button>
                `,
              )}
            </div>
          `
        : null}

      ${effectiveTab === 'overview'
        ? html`
            <${OverviewTab}
              machine=${machine}
              nodeInfo=${nodeInfo}
              link=${link}
              allowedSet=${allowedSet}
              technical=${technical}
              wide=${wide}
              hideWideChrome=${hideWideChrome}
              wakePending=${wakePending}
              capsStore=${capsStore}
              onRegisterRefresh=${onRegisterRefresh}
              onEnsureAwake=${onEnsureAwake}
              onStopJob=${handleStopJob}
              onRefreshFleet=${onRefreshFleet}
              onUpdateMachineSys=${onUpdateMachineSys}
            />
          `
        : effectiveTab === 'more'
          ? html`<${MoreTab} machine=${machine} />`
          : effectiveTab === 'command'
            ? html`
                <div class="spaced">
                  <div class="main command-stream">
                    <${CommandStream}
                      machine=${machine}
                      runs=${runs}
                      technical=${technical}
                      allowedSet=${allowedSet}
                      nodeInfo=${nodeInfo}
                      onOpenJob=${openJob}
                    />
                  </div>
                  ${(machine.reachable || (machine.canWake && allowedSet?.has('power.wake'))) && commandRuns
                    ? html`
                        <div class="spaced">
                          <${CommandComposer}
                            machine=${machine}
                            nodeInfo=${nodeInfo}
                            link=${link}
                            allowedSet=${allowedSet}
                            state=${commandRuns.draft}
                            onStateChange=${onCommandRunsChange.setDraft}
                            onAppendRun=${onCommandRunsChange.appendRun}
                            onUpdateRun=${onCommandRunsChange.updateRun}
                            onOpenJob=${openJob}
                            onEnsureAwake=${onEnsureAwake}
                          />
                        </div>
                      `
                    : null}
                </div>
              `
            : effectiveTab === 'files'
              ? html`
                  <${MachineFilesBrowser}
                    machine=${machine}
                    nodeInfo=${nodeInfo}
                    tools=${tools}
                    allowedSet=${allowedSet}
                    link=${link}
                    capsStore=${capsStore}
                    folderChanges=${folderChanges}
                    onRegisterRefresh=${onRegisterRefresh}
                    onRefreshFleet=${onRefreshFleet}
                    settings=${settings}
                    deviceNoun=${deviceNoun}
                    thumbCache=${thumbCache}
                    wide=${wide}
                    filesState=${filesState}
                    filesActions=${filesActions}
                    onEnsureAwake=${onEnsureAwake}
                  />
                `
              : effectiveTab === 'activity'
                ? html`
                    <${MachineActivityTab}
                      machine=${machine}
                      nodeInfo=${nodeInfo}
                      link=${link}
                      allowedSet=${allowedSet}
                      technical=${technical}
                      jobId=${jobId}
                      onObserveTransferJobs=${onObserveTransferJobs}
                      onRegisterRefresh=${onRegisterRefresh}
                      onRefreshFleet=${onRefreshFleet}
                    />
                  `
                : effectiveTab === 'services'
                  ? html`
                      <${ServicesTab}
                        machine=${machine}
                        nodeInfo=${nodeInfo}
                        link=${link}
                        allowedSet=${allowedSet}
                        onRegisterRefresh=${onRegisterRefresh}
                      />
                    `
                  : effectiveTab === 'projects'
                    ? html`
                        <${ProjectsTab}
                          machine=${machine}
                          nodeInfo=${nodeInfo}
                          link=${link}
                          allowedSet=${allowedSet}
                          onRegisterRefresh=${onRegisterRefresh}
                        />
                      `
                    : html`
                        <${PowerTab}
                          machine=${machine}
                          nodeInfo=${nodeInfo}
                          link=${link}
                          allowedSet=${allowedSet}
                          technical=${technical}
                          wakePending=${wakePending}
                          onRegisterRefresh=${onRegisterRefresh}
                          onEnsureAwake=${onEnsureAwake}
                          onWakeSent=${onWakeSent}
                          onRequestConfirm=${onRequestConfirm}
                          onRefreshFleet=${onRefreshFleet}
                        />
                      `}
    </div>
  `;
}
