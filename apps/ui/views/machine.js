// One machine screen with Overview, Run, Files, Jobs, Services, Projects, and Power tabs.

import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { CopyButton, Meter, Notice } from './common.js';
import {
  diskUsagePct,
  formatBytePair,
  formatBytes,
  formatDuration,
  formatDurationMs,
  formatElapsed,
  formatRelativeTime,
  machineMetrics,
  osLabel,
  realDisks,
} from '../lib/format.js';
import { MACHINE_TABS, routeHref } from '../lib/route.js';
import { describePowerConfirm, unavailableReason } from '../lib/schema.js';

const TAB_LABELS = {
  overview: 'Overview',
  run: 'Run',
  files: 'Files',
  jobs: 'Jobs',
  services: 'Services',
  projects: 'Projects',
  power: 'Power',
};

function targetFor(machineName, nodeName) {
  return machineName && machineName !== nodeName ? machineName : undefined;
}

function defaultRoot(whoami) {
  const roots = whoami?.paths;
  if (Array.isArray(roots) && roots.length > 0) {
    return roots[0].replace(/\/\*\*$/, '').replace(/\/\*$/, '') || '/';
  }
  return '~';
}

function splitCrumbs(fullPath) {
  if (!fullPath) return [];
  if (fullPath === '/' || fullPath === '~') return [{ label: fullPath, path: fullPath }];
  if (/^[A-Za-z]:\\?/.test(fullPath)) {
    const parts = fullPath.split(/\\+/).filter(Boolean);
    const out = [];
    let acc = '';
    for (let i = 0; i < parts.length; i += 1) {
      acc = i === 0 ? `${parts[i]}\\` : `${acc}${acc.endsWith('\\') ? '' : '\\'}${parts[i]}`;
      out.push({ label: parts[i], path: acc });
    }
    return out;
  }
  const parts = fullPath.split('/').filter(Boolean);
  const out = [{ label: '/', path: '/' }];
  let acc = '';
  for (const part of parts) {
    acc += `/${part}`;
    out.push({ label: part, path: acc });
  }
  return out;
}

// ── Overview tab ─────────────────────────────────────────────────────────────────────────────

function OverviewTab({ machine, allowedSet, onWake }) {
  if (!machine.reachable) {
    const err = machine.error || {
      message: machine.reason || `${machine.name} did not answer.`,
      hint: 'Check that the machine is awake and reachable.',
    };
    return html`
      <${Notice}
        kind="warn"
        title=${err.message || `${machine.name} did not answer.`}
        hint=${err.hint || null}
      />
      ${machine.canWake && allowedSet?.has('power.wake')
        ? html`
            <div class="actions">
              <button type="button" class="btn primary" onClick=${() => onWake(machine)}>
                Wake ${machine.name}
              </button>
            </div>
          `
        : null}
    `;
  }

  const sys = machine.sys;
  const metrics = machineMetrics(sys);
  const disks = realDisks(sys?.disks);
  const loadText =
    Array.isArray(sys?.cpu?.load) && sys.cpu.load.length > 0
      ? `load ${sys.cpu.load[0].toFixed(1)}`
      : null;
  const cpuDetail = [
    metrics ? `${metrics.cpuPct}%` : null,
    loadText,
    sys?.cpu?.cores ? `${sys.cpu.cores} cores` : null,
  ]
    .filter(Boolean)
    .join(' · ');

  const busySummary = machine.busy
    ? machine.runningJobs > 0
      ? `yes: ${machine.runningJobs} ${machine.runningJobs === 1 ? 'job' : 'jobs'}`
      : machine.busyReasons?.length
        ? `yes: ${machine.busyReasons.join(', ')}`
        : 'yes'
    : 'no';

  const systemLine = [
    sys?.os?.family ? osLabel(sys.os.family) : null,
    sys?.os?.kernel || sys?.os?.version || null,
    sys?.os?.arch ? `· ${sys.os.arch}` : null,
  ]
    .filter(Boolean)
    .join(' ')
    .replace(' ·', ' ·');

  return html`
    <div>
      ${metrics
        ? html`
            <div class="list">
              <div class="gauge">
                <header>
                  <span>Processor</span>
                  <span>${cpuDetail}</span>
                </header>
                <${Meter} value=${metrics.cpuPct} />
              </div>
              <div class="gauge">
                <header>
                  <span>Memory</span>
                  <span>${formatBytePair(metrics.memUsed, metrics.memTotal)}</span>
                </header>
                <${Meter} value=${metrics.memPct} />
              </div>
              ${disks.map((disk) => {
                const used = Math.max(0, disk.total - (disk.available ?? 0));
                const pct = diskUsagePct(disk);
                return html`
                  <div class="gauge" key=${disk.mount}>
                    <header>
                      <span>Disk <span class="mono dim">${disk.mount}</span></span>
                      <span>${formatBytePair(used, disk.total)}</span>
                    </header>
                    <${Meter} value=${pct} />
                  </div>
                `;
              })}
              ${metrics.gpus.map(
                (gpu, idx) => html`
                  <div class="gauge" key=${`${gpu.name}-${idx}`}>
                    <header>
                      <span>Graphics memory</span>
                      <span>${formatBytePair(gpu.used, gpu.total)}</span>
                    </header>
                    <${Meter} value=${gpu.pct} />
                  </div>
                `,
              )}
            </div>
          `
        : html`<div class="empty">Loading system details…</div>`}

      <p class="label">About</p>
      <div class="list">
        <dl class="kv">
          <dt>system</dt>
          <dd>${systemLine || 'Unknown'}</dd>
          ${sys?.uptime_s != null
            ? html`
                <dt>up</dt>
                <dd>${formatDuration(sys.uptime_s)}</dd>
              `
            : null}
          ${machine.version
            ? html`
                <dt>legion</dt>
                <dd>${machine.version}</dd>
              `
            : null}
          <dt>busy</dt>
          <dd>${busySummary}</dd>
        </dl>
      </div>
    </div>
  `;
}

// ── Run tab ──────────────────────────────────────────────────────────────────────────────────

function RunTab({ machine, nodeInfo, link, allowedSet, history, onRecordRun, onOpenJob }) {
  const [cmd, setCmd] = useState('');
  const [asJob, setAsJob] = useState(false);
  const [running, setRunning] = useState(false);
  const [chunks, setChunks] = useState([]);
  const [metaLine, setMetaLine] = useState('');
  const [error, setError] = useState(null);
  const [startedJob, setStartedJob] = useState(null);
  const abortRef = useRef(null);

  const canExec = allowedSet ? allowedSet.has('exec.run') : true;
  const canJob = allowedSet ? allowedSet.has('job.start') : true;
  const on = targetFor(machine.name, nodeInfo?.node);

  useEffect(() => {
    return () => {
      if (abortRef.current) abortRef.current.abort();
    };
  }, [machine.name]);

  async function runCommand(line) {
    const trimmed = String(line || '').trim();
    if (!trimmed) return;
    setError(null);
    setStartedJob(null);

    if (asJob) {
      setRunning(true);
      try {
        const res = await link.call('job.start', { cmd: trimmed }, { on });
        setStartedJob(res);
        onRecordRun(machine.name, { cmd: trimmed, jobId: res.id, at: Date.now() });
      } catch (err) {
        setError(err);
      } finally {
        setRunning(false);
      }
      return;
    }

    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    setRunning(true);
    setChunks([]);
    setMetaLine(`${machine.name} · running…`);
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
              setChunks((prev) => [...prev, { kind: evt.kind, text: String(evt.data ?? '') }]);
            }
          },
        },
      );
      if (!streamed) {
        const fallback = [];
        if (res.stdout) fallback.push({ kind: 'stdout', text: res.stdout });
        if (res.stderr) fallback.push({ kind: 'stderr', text: res.stderr });
        setChunks(fallback);
      }
      const statusText = res.timed_out
        ? 'timed out'
        : res.exit_code != null
          ? `exit ${res.exit_code}`
          : res.signal != null
            ? `signal ${res.signal}`
            : 'done';
      setMetaLine(`${machine.name} · ${statusText} · ${formatDurationMs(res.duration_ms ?? 0)}`);
      onRecordRun(machine.name, {
        cmd: trimmed,
        exitCode: res.exit_code,
        durationMs: res.duration_ms,
        at: Date.now(),
      });
    } catch (err) {
      if (err?.code === 'cancelled') {
        setMetaLine(`${machine.name} · cancelled`);
      } else {
        setError(err);
        setMetaLine('');
      }
    } finally {
      abortRef.current = null;
      setRunning(false);
    }
  }

  function onSubmit(e) {
    e.preventDefault();
    if (running && abortRef.current) {
      abortRef.current.abort();
      return;
    }
    runCommand(cmd);
  }

  if (!canExec && !canJob) {
    return html`
      <${Notice}
        title="Running commands is not allowed."
        hint=${unavailableReason({ name: 'exec.run', effect: 'exec' }, nodeInfo?.whoami)}
      />
    `;
  }

  const machineHistory = (history || []).filter((item) => item.machine === machine.name);

  return html`
    <div>
      <form class="prompt" onSubmit=${onSubmit}>
        <input
          type="text"
          value=${cmd}
          onInput=${(e) => setCmd(e.target.value)}
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
            <label class="switch">
              <span>
                Keep running after I leave
                <small class="help">Starts it as a job you can come back to.</small>
              </span>
              <input
                type="checkbox"
                checked=${asJob}
                onChange=${(e) => setAsJob(e.target.checked)}
              />
            </label>
          `
        : null}

      ${error ? html`<${Notice} error=${error} />` : null}

      ${startedJob
        ? html`
            <div class="notice">
              <strong>Started job ${startedJob.name || startedJob.id}</strong>
              <span>
                Running in the background on ${machine.name}.
              </span>
              <div class="row-actions">
                <button
                  type="button"
                  class="btn small primary"
                  onClick=${() => onOpenJob(startedJob.id)}
                >
                  Follow output
                </button>
              </div>
            </div>
          `
        : null}

      ${metaLine || chunks.length > 0
        ? html`
            <pre class="out">
${metaLine ? html`<span class="meta">${metaLine}${'\n'}</span>` : null}${chunks.map(
              (c, i) =>
                c.kind === 'stderr'
                  ? html`<span key=${i} class="err">${c.text}</span>`
                  : html`<span key=${i}>${c.text}</span>`,
            )}</pre>
          `
        : null}

      ${machineHistory.length > 0
        ? html`
            <p class="label">Earlier</p>
            <div class="list">
              ${machineHistory.map(
                (item, idx) => html`
                  <button
                    type="button"
                    key=${`${item.at}-${idx}`}
                    class="row"
                    onClick=${() => {
                      setCmd(item.cmd);
                      if (!item.jobId) runCommand(item.cmd);
                      else onOpenJob(item.jobId);
                    }}
                  >
                    <span class="body">
                      <span class="title mono">${item.cmd}</span>
                      <span class="sub">
                        ${item.jobId
                          ? `job ${item.jobId}`
                          : item.exitCode != null
                            ? `exit ${item.exitCode}`
                            : 'ran'}${item.at ? ` · ${formatRelativeTime(item.at)}` : ''}
                      </span>
                    </span>
                  </button>
                `,
              )}
            </div>
          `
        : null}
    </div>
  `;
}

// ── Files tab ────────────────────────────────────────────────────────────────────────────────

const PREVIEW_CAP = 131072;

function FilesTab({ machine, nodeInfo, link }) {
  const initialPath = defaultRoot(nodeInfo?.whoami);
  const [dirPath, setDirPath] = useState(initialPath);
  const [listing, setListing] = useState(null);
  const [preview, setPreview] = useState(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState(null);
  const crumbsRef = useRef(null);
  const on = targetFor(machine.name, nodeInfo?.node);

  async function loadDir(path) {
    setLoading(true);
    setError(null);
    setPreview(null);
    try {
      const res = await link.call('fs.list', { path, depth: 1 }, { on });
      setListing(res);
      setDirPath(res.path || path);
    } catch (err) {
      setError(err);
    } finally {
      setLoading(false);
    }
  }

  async function openFile(entry) {
    setLoading(true);
    setError(null);
    try {
      const res = await link.call(
        'fs.read',
        { path: entry.path, offset: 0, length: PREVIEW_CAP },
        { on },
      );
      setPreview({ entry, data: res });
    } catch (err) {
      setError(err);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    loadDir(defaultRoot(nodeInfo?.whoami));
  }, [machine.name]);

  const currentPath = listing?.path || dirPath;
  const crumbs = splitCrumbs(currentPath);

  useEffect(() => {
    if (crumbsRef.current) {
      crumbsRef.current.scrollLeft = crumbsRef.current.scrollWidth;
    }
  }, [currentPath]);

  const filesHref = routeHref({ section: 'machines', machine: machine.name, tab: 'files' });

  return html`
    <div>
      <div class="crumbs-bar">
        <nav ref=${crumbsRef} class="crumbs" aria-label="Directory path">
          ${crumbs.map(
            (c, i) => html`
              <span key=${c.path} class="crumb-item">
                ${i > 0 && crumbs[i - 1].label !== '/'
                  ? html`<span class="dim" aria-hidden="true">/</span>`
                  : null}
                <a
                  href=${filesHref}
                  onClick=${(e) => {
                    e.preventDefault();
                    loadDir(c.path);
                  }}
                >
                  ${c.label}
                </a>
              </span>
            `,
          )}
        </nav>
        <${CopyButton}
          text=${preview ? preview.data.path : currentPath}
          label="Copy path"
          iconOnly=${true}
        />
      </div>

      ${error ? html`<${Notice} error=${error} />` : null}

      ${preview
        ? html`
            <div>
              <div class="result-head">
                <p class="label">${preview.entry.name} · ${formatBytes(preview.data.size)}</p>
                <button type="button" class="btn small quiet" onClick=${() => setPreview(null)}>
                  Back to folder
                </button>
              </div>
              ${preview.data.encoding === 'base64'
                ? html`
                    <${Notice}
                      kind="warn"
                      title="Binary file preview is not shown."
                      hint=${`${formatBytes(preview.data.size)} at ${preview.data.path}.`}
                    />
                  `
                : html`
                    ${!preview.data.eof
                      ? html`
                          <${Notice}
                            kind="warn"
                            title="Showing the start of this file."
                            hint=${`Cut at ${formatBytes(preview.data.length)} of ${formatBytes(preview.data.size)}.`}
                          />
                        `
                      : null}
                    <pre class="out">${preview.data.content}</pre>
                  `}
            </div>
          `
        : listing
          ? html`
              ${listing.entries.length === 0
                ? html`<div class="empty">This folder is empty.</div>`
                : html`
                    <div class="list">
                      ${listing.entries.map(
                        (entry) => html`
                          <button
                            type="button"
                            key=${entry.path}
                            class="row"
                            onClick=${() =>
                              entry.kind === 'dir' ? loadDir(entry.path) : openFile(entry)}
                          >
                            <span class="body">
                              <span class="title mono">
                                ${entry.kind === 'dir' ? `${entry.name}/` : entry.name}
                              </span>
                              <span class="sub">
                                ${entry.kind}
                                ${entry.mtime ? ` · ${formatRelativeTime(entry.mtime)}` : ''}
                              </span>
                            </span>
                            <span class="end">
                              ${entry.kind === 'file' ? formatBytes(entry.size) : ''}
                            </span>
                          </button>
                        `,
                      )}
                    </div>
                  `}
            `
          : loading
            ? html`<div class="empty">Loading files…</div>`
            : null}
    </div>
  `;
}

// ── Jobs tab (also reused by Jobs screen) ────────────────────────────────────────────────────

export function JobFollowView({ machineName, nodeName, jobId, link, allowedSet, onBack, onChanged }) {
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

  const active = status && (status.state === 'starting' || status.state === 'running');

  return html`
    <div>
      <div class="result-head">
        <p class="label">${status?.name || jobId} · ${status?.state || 'loading…'}</p>
        <div class="result-tools">
          ${active && canCancel
            ? html`
                <button
                  type="button"
                  class="btn small danger"
                  disabled=${stopping}
                  onClick=${onStop}
                >
                  ${stopping ? 'Stopping…' : 'Stop job'}
                </button>
              `
            : null}
          ${onBack
            ? html`<button type="button" class="btn small" onClick=${onBack}>All jobs</button>`
            : null}
        </div>
      </div>

      ${error ? html`<${Notice} error=${error} />` : null}

      ${status
        ? html`
            <div class="list">
              <dl class="kv">
                <dt>machine</dt>
                <dd>${machineName}</dd>
                <dt>state</dt>
                <dd>${status.state}${status.exit_code != null ? ` (exit ${status.exit_code})` : ''}</dd>
                ${status.cmd || status.argv
                  ? html`
                      <dt>command</dt>
                      <dd>${status.cmd || status.argv.join(' ')}</dd>
                    `
                  : null}
                ${status.duration_s != null
                  ? html`
                      <dt>duration</dt>
                      <dd>${formatElapsed(status.duration_s)}</dd>
                    `
                  : null}
              </dl>
            </div>
          `
        : null}

      <p class="label">Output</p>
      <pre class="out wrap">${logText || (active ? 'Waiting for output…' : 'No output recorded.')}</pre>
    </div>
  `;
}

function JobsTab({ machine, nodeInfo, link, allowedSet, jobId, onOpenJob, onCloseJob, onRefreshFleet }) {
  const [jobs, setJobs] = useState(null);
  const [error, setError] = useState(null);
  const on = targetFor(machine.name, nodeInfo?.node);

  async function load() {
    setError(null);
    try {
      const res = await link.call('job.list', { limit: 50 }, { on });
      const list = Array.isArray(res?.jobs) ? res.jobs : [];
      list.sort((a, b) => {
        const aRun = a.state === 'running' || a.state === 'starting' ? 0 : 1;
        const bRun = b.state === 'running' || b.state === 'starting' ? 0 : 1;
        return aRun - bRun;
      });
      setJobs(list);
    } catch (err) {
      setError(err);
    }
  }

  useEffect(() => {
    load();
  }, [machine.name, jobId]);

  if (jobId) {
    return html`
      <${JobFollowView}
        machineName=${machine.name}
        nodeName=${nodeInfo?.node}
        jobId=${jobId}
        link=${link}
        allowedSet=${allowedSet}
        onBack=${onCloseJob}
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
      ${jobs === null
        ? html`<div class="empty">Loading jobs…</div>`
        : jobs.length === 0
          ? html`<div class="empty">No jobs recorded on ${machine.name}.</div>`
          : html`
              <div class="list">
                ${jobs.map((job) => {
                  const active = job.state === 'running' || job.state === 'starting';
                  return html`
                    <button
                      type="button"
                      key=${job.id}
                      class="row"
                      onClick=${() => onOpenJob(job.id)}
                    >
                      <span class=${active ? 'dot busy' : 'dot off'} aria-hidden="true"></span>
                      <span class="body">
                        <span class="title mono">${job.command || job.name || job.id}</span>
                        <span class="sub">
                          ${job.state}
                          ${job.exit_code != null ? ` · exit ${job.exit_code}` : ''}
                          ${job.created ? ` · ${formatRelativeTime(job.created)}` : ''}
                        </span>
                      </span>
                    </button>
                  `;
                })}
              </div>
            `}
    </div>
  `;
}

// ── Services tab ─────────────────────────────────────────────────────────────────────────────

function ServicesTab({ machine, nodeInfo, link, allowedSet }) {
  const [services, setServices] = useState(null);
  const [filter, setFilter] = useState('');
  const [selected, setSelected] = useState(null);
  const [logs, setLogs] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const on = targetFor(machine.name, nodeInfo?.node);

  async function load(q = filter) {
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
  }, [machine.name]);

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
          class="input"
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

function ProjectsTab({ machine, nodeInfo, link, allowedSet }) {
  const [projects, setProjects] = useState(null);
  const [output, setOutput] = useState('');
  const [busyKey, setBusyKey] = useState('');
  const [error, setError] = useState(null);
  const on = targetFor(machine.name, nodeInfo?.node);

  async function load() {
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
  }, [machine.name]);

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
      if (res?.stdout && !output) setOutput(res.stdout + (res.stderr || ''));
      else if (res?.id) setOutput(`Started job ${res.id}`);
    } catch (err) {
      setError(err);
    } finally {
      setBusyKey('');
    }
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

function PowerTab({ machine, nodeInfo, link, allowedSet, onRequestConfirm, onRefreshFleet }) {
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

  async function runAction(tool, args, target, hasDry) {
    setError(null);
    setResult(null);
    const liveJobs = Array.isArray(machine.jobs) ? machine.jobs : [];
    const busyReasons = status?.busy?.state === 'busy' ? status.busy.reasons || [] : machine.busyReasons || [];
    const copy = describePowerConfirm(tool, machine.name, args, liveJobs, busyReasons);
    onRequestConfirm({
      title: copy.title,
      summary: copy.summary,
      jobs: tool === 'power.wake' ? [] : liveJobs,
      target: target || nodeInfo?.node,
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

  if (!machine.reachable) {
    const canWake = allowedSet?.has('power.wake');
    const err = machine.error || {
      message: machine.reason || `${machine.name} did not answer.`,
      hint: machine.canWake
        ? 'You can send a Wake-on-LAN packet from a reachable helper.'
        : 'No Wake-on-LAN address is configured for this machine in fleet.toml.',
    };
    return html`
      <div>
        <${Notice}
          kind="warn"
          title=${err.message || `${machine.name} did not answer.`}
          hint=${err.hint || null}
        />
        ${machine.canWake
          ? html`
              <div class="list">
                <div class=${canWake ? 'row' : 'row off'}>
                  <div class="body">
                    <div class="title">Wake ${machine.name}</div>
                    <div class="sub wrap">
                      ${canWake
                        ? 'Send Wake-on-LAN magic packets for this machine.'
                        : unavailableReason({ name: 'power.wake', effect: 'power' }, nodeInfo?.whoami)}
                    </div>
                  </div>
                  <button
                    type="button"
                    class="btn small"
                    disabled=${!canWake || busy}
                    onClick=${() =>
                      runAction('power.wake', { machine: machine.name }, undefined, true)}
                  >
                    Wake
                  </button>
                </div>
              </div>
            `
          : null}
        ${error ? html`<${Notice} error=${error} />` : null}
        ${result ? html`<pre class="out wrap">${JSON.stringify(result, null, 2)}</pre>` : null}
      </div>
    `;
  }

  const actions = [
    {
      tool: 'power.sleep',
      label: `Sleep ${machine.name}`,
      verb: 'Sleep',
      desc: `Put ${machine.name} to sleep after about 3 seconds.`,
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
      desc: `Arm boot target "${t.name}" (${t.how}) once and restart ${machine.name}.`,
      args: { into: t.name },
      dry: true,
    })),
    {
      tool: 'power.shutdown',
      label: `Shut down ${machine.name}`,
      verb: 'Shut down',
      desc: `Power off ${machine.name} after a short delay.`,
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
        ? html`
            <div class="list">
              <dl class="kv">
                <dt>state</dt>
                <dd>
                  ${status.busy?.state || 'unknown'}
                  ${status.busy?.reasons?.length ? ` (${status.busy.reasons.join(', ')})` : ''}
                </dd>
                ${status.system
                  ? html`
                      <dt>system</dt>
                      <dd>${status.system}</dd>
                    `
                  : null}
                <dt>sudo</dt>
                <dd>${status.sudo ? 'yes' : 'no'}</dd>
              </dl>
            </div>
          `
        : !error
          ? html`<div class="empty">Loading power state…</div>`
          : null}

      <p class="label">Power actions</p>
      <div class="list">
        ${actions.map((item) => {
          const allowed = allowedSet ? allowedSet.has(item.tool) : true;
          return html`
            <div class=${allowed ? 'row' : 'row off'} key=${`${item.tool}:${item.label}`}>
              <div class="body">
                <div class="title">
                  <span>${item.label}</span>
                  <span class="chip fx power">power</span>
                </div>
                <div class="sub wrap">
                  ${allowed
                    ? item.desc
                    : unavailableReason({ name: item.tool, effect: 'power' }, nodeInfo?.whoami)}
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

      ${result ? html`<pre class="out wrap">${JSON.stringify(result, null, 2)}</pre>` : null}
    </div>
  `;
}

// ── MachineDetail shell ──────────────────────────────────────────────────────────────────────

export function MachineDetail({
  machine,
  tab,
  jobId,
  showPaneTitle,
  link,
  nodeInfo,
  allowedSet,
  onRequestConfirm,
  onRefreshFleet,
  runHistory,
  onRecordRun,
}) {
  const dotClass = !machine.reachable ? 'dot off' : machine.busy ? 'dot busy' : 'dot';

  function selectTab(nextTab) {
    window.location.hash = routeHref({ section: 'machines', machine: machine.name, tab: nextTab });
  }

  function openJob(id) {
    window.location.hash = routeHref({
      section: 'machines',
      machine: machine.name,
      tab: 'jobs',
      jobId: id,
    });
  }

  function closeJob() {
    window.location.hash = routeHref({ section: 'machines', machine: machine.name, tab: 'jobs' });
  }

  return html`
    <div>
      ${showPaneTitle
        ? html`<h2 class="pane-title"><span class=${dotClass} aria-hidden="true"></span>${machine.name}</h2>`
        : null}
      <div class="tabs" role="tablist" aria-label=${`${machine.name} sections`}>
        ${MACHINE_TABS.map(
          (id) => html`
            <button
              type="button"
              key=${id}
              role="tab"
              aria-selected=${tab === id ? 'true' : 'false'}
              onClick=${() => selectTab(id)}
            >
              ${TAB_LABELS[id]}
            </button>
          `,
        )}
      </div>

      ${tab === 'overview'
        ? html`
            <${OverviewTab}
              machine=${machine}
              allowedSet=${allowedSet}
              onWake=${() => selectTab('power')}
            />
          `
        : tab === 'run'
          ? html`
              <${RunTab}
                machine=${machine}
                nodeInfo=${nodeInfo}
                link=${link}
                allowedSet=${allowedSet}
                history=${runHistory}
                onRecordRun=${onRecordRun}
                onOpenJob=${openJob}
              />
            `
          : tab === 'files'
            ? html`<${FilesTab} machine=${machine} nodeInfo=${nodeInfo} link=${link} />`
            : tab === 'jobs'
              ? html`
                  <${JobsTab}
                    machine=${machine}
                    nodeInfo=${nodeInfo}
                    link=${link}
                    allowedSet=${allowedSet}
                    jobId=${jobId}
                    onOpenJob=${openJob}
                    onCloseJob=${closeJob}
                    onRefreshFleet=${onRefreshFleet}
                  />
                `
              : tab === 'services'
                ? html`
                    <${ServicesTab}
                      machine=${machine}
                      nodeInfo=${nodeInfo}
                      link=${link}
                      allowedSet=${allowedSet}
                    />
                  `
                : tab === 'projects'
                  ? html`
                      <${ProjectsTab}
                        machine=${machine}
                        nodeInfo=${nodeInfo}
                        link=${link}
                        allowedSet=${allowedSet}
                      />
                    `
                  : html`
                      <${PowerTab}
                        machine=${machine}
                        nodeInfo=${nodeInfo}
                        link=${link}
                        allowedSet=${allowedSet}
                        onRequestConfirm=${onRequestConfirm}
                        onRefreshFleet=${onRefreshFleet}
                      />
                    `}
    </div>
  `;
}
