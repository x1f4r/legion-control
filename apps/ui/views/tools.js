// Technical All tools view and schema-driven tool runner (reused by Actions and Settings).

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { CodeText, Notice, ResultView } from './common.js';
import { ScrollContainer } from './scroll-container.js';
import { parentFilePath } from '../lib/files.js';
import { parseTransferTarget } from '../lib/folder-changes.js';
import { splitFirstSentence } from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import {
  buildArgs,
  describePowerConfirm,
  describeSchema,
  groupToolsByFamily,
  hasDryRun,
  initialValues,
  isEffectful,
  needsConfirmation,
  parseRawArgs,
  resolveToolTarget,
  unavailableReason,
  valuesFromArgs,
} from '../lib/schema.js';

function FieldHelp({ description }) {
  if (!description) return null;
  const { first, rest } = splitFirstSentence(description);
  return html`
    <small><${CodeText} text=${first} /></small>
    ${rest
      ? html`
          <details class="more">
            <summary>More</summary>
            <p><${CodeText} text=${rest} /></p>
          </details>
        `
      : null}
  `;
}

function FieldInput({ field, value, onChange, error, idPrefix }) {
  const id = `${idPrefix}-${field.name}`;
  const placeholder =
    field.defaultValue !== undefined ? String(field.defaultValue) : undefined;
  const helpParts = splitFirstSentence(field.description);

  if (field.kind === 'boolean') {
    return html`
      <div>
        <label class="switch" htmlFor=${id}>
          <span>
            <span class="mono">${field.name}</span>
            ${field.required ? html` <em class="dim">required</em>` : null}
            ${helpParts.first
              ? html`<small class="help"><${CodeText} text=${helpParts.first} /></small>`
              : null}
          </span>
          <input
            id=${id}
            type="checkbox"
            checked=${Boolean(value)}
            onChange=${(e) => onChange(e.target.checked)}
          />
        </label>
        ${helpParts.rest
          ? html`
              <details class="more">
                <summary>More</summary>
                <p><${CodeText} text=${helpParts.rest} /></p>
              </details>
            `
          : null}
        ${error ? html`<small class="field-err">${error}</small>` : null}
      </div>
    `;
  }

  if (field.kind === 'object') {
    const obj = value && typeof value === 'object' ? value : {};
    return html`
      <div class="field">
        <span>
          ${field.name}
          <em>${field.required ? 'required' : 'optional'}</em>
        </span>
        <${FieldHelp} description=${field.description} />
        <div class="nested">
          ${field.fields.map(
            (sub) => html`
              <${FieldInput}
                key=${sub.name}
                field=${sub}
                value=${obj[sub.name]}
                onChange=${(nextVal) => onChange({ ...obj, [sub.name]: nextVal })}
                idPrefix=${id}
              />
            `,
          )}
        </div>
        ${error ? html`<small class="field-err">${error}</small>` : null}
      </div>
    `;
  }

  return html`
    <div class="field">
      <label htmlFor=${id} class="field-label">
        <span>
          ${field.name}
          <em>${field.required ? 'required' : 'optional'}</em>
        </span>
      </label>
      ${field.kind === 'enum'
        ? html`
            <select id=${id} value=${value ?? ''} onChange=${(e) => onChange(e.target.value)}>
              <option value="">${placeholder ? `Default (${placeholder})` : 'Choose…'}</option>
              ${field.enumValues.map(
                (opt) => html`<option key=${opt} value=${opt}>${opt}</option>`,
              )}
            </select>
          `
        : field.kind === 'string-array' || field.kind === 'json'
          ? html`
              <textarea
                id=${id}
                value=${value ?? ''}
                onInput=${(e) => onChange(e.target.value)}
                placeholder=${field.kind === 'string-array' ? 'One item per line' : '{}'}
              ></textarea>
            `
          : html`
              <input
                id=${id}
                type="text"
                inputMode=${field.kind === 'integer'
                  ? 'numeric'
                  : field.kind === 'number'
                    ? 'decimal'
                    : undefined}
                value=${value ?? ''}
                onInput=${(e) => onChange(e.target.value)}
                placeholder=${placeholder}
                autocomplete="off"
                spellcheck="false"
              />
            `}
      <${FieldHelp} description=${field.description} />
      ${error ? html`<small class="field-err">${error}</small>` : null}
    </div>
  `;
}

export function ToolDetail({
  tool,
  allowed,
  initialTarget = 'local',
  hideTargetPicker = false,
  nodeInfo,
  machines,
  runningJobs,
  link,
  folderChanges = null,
  technical = true,
  onRequestConfirm,
  onEnsureAwake,
}) {
  const fields = useMemo(() => describeSchema(tool.input_schema), [tool.name]);
  const descParts = useMemo(() => splitFirstSentence(tool.description), [tool.description]);
  const [values, setValues] = useState(() => initialValues(tool.input_schema));
  const [rawMode, setRawMode] = useState(false);
  const [rawText, setRawText] = useState('{}');
  const [targetMode, setTargetMode] = useState(initialTarget || 'local');
  const [multiTargets, setMultiTargets] = useState([]);
  const [fieldErrors, setFieldErrors] = useState({});
  const [running, setRunning] = useState(false);
  const [events, setEvents] = useState([]);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(null);
  const abortRef = useRef(null);

  const localNode = nodeInfo?.node || 'this node';
  const machineList = machines || [];
  const machineNames = machineList.map((m) => m.name);
  const remoteMachines = machineList.filter((m) => m.name !== localNode);
  const answeringNames = machineList
    .filter((m) => m.name === localNode || m.reachable)
    .map((m) => m.name);
  const skippedNames = machineList
    .filter((m) => m.name !== localNode && !m.reachable)
    .map((m) => m.name);
  const isSingleRemote =
    targetMode !== 'local' && targetMode !== 'all' && targetMode !== 'multi';

  useEffect(() => {
    return () => {
      if (abortRef.current) {
        abortRef.current.abort();
        abortRef.current = null;
      }
    };
  }, []);

  function toggleRaw(nextRaw) {
    setError(null);
    setFieldErrors({});
    if (nextRaw) {
      const built = buildArgs(tool.input_schema, values);
      setRawText(JSON.stringify(built.args || {}, null, 2));
      setRawMode(true);
    } else {
      const parsed = parseRawArgs(rawText);
      if (!parsed.ok) {
        setError({ code: 'invalid_args', message: parsed.error, hint: null });
        return;
      }
      setValues(valuesFromArgs(tool.input_schema, parsed.args));
      setRawMode(false);
    }
  }

  function currentTarget() {
    return resolveToolTarget({
      targetMode,
      effect: tool.effect,
      localNode,
      answeringNames,
      skippedNames,
      multiTargets,
    });
  }

  function collectArgs() {
    if (rawMode) {
      const parsed = parseRawArgs(rawText);
      if (!parsed.ok) {
        setError({ code: 'invalid_args', message: parsed.error, hint: null });
        return null;
      }
      setFieldErrors({});
      return parsed.args;
    }
    const built = buildArgs(tool.input_schema, values);
    if (!built.ok) {
      setFieldErrors(built.errors);
      setError({
        code: 'invalid_args',
        message: 'Check the highlighted fields before running.',
        hint: null,
      });
      return null;
    }
    setFieldErrors({});
    return built.args;
  }

  function findOfflineMachineForCall(args, explicitOn) {
    if (tool.name === 'power.wake') return null;
    const byName = new Map(machineList.map((m) => [m.name, m]));
    if (typeof explicitOn === 'string' && explicitOn !== 'all') {
      const targetM = byName.get(explicitOn);
      if (targetM && !targetM.reachable) {
        return { machine: targetM, kind: 'action', label: `run ${tool.name}` };
      }
    }
    if (tool.name === 'fs.transfer' && args && typeof args === 'object') {
      const fromVal = typeof args.from === 'string' ? args.from : '';
      const rawPath = fromVal.includes(':') ? fromVal.slice(fromVal.indexOf(':') + 1) : fromVal;
      const baseName = rawPath.replace(/\/+$/, '').split('/').pop() || 'files';
      const verb = args.move ? 'move' : 'send';
      for (const key of ['from', 'to']) {
        const val = typeof args[key] === 'string' ? args[key] : '';
        const colon = val.indexOf(':');
        if (colon > 0) {
          const m = byName.get(val.slice(0, colon));
          if (m && !m.reachable) {
            const prep = key === 'to' ? 'to' : 'from';
            return { machine: m, kind: 'transfer', label: `${verb} ${baseName} ${prep} ${m.name}` };
          }
        }
      }
    }
    return null;
  }

  async function executeCall(args, explicitOn) {
    const on = explicitOn !== undefined ? explicitOn : currentTarget().on;
    const offline = onEnsureAwake ? findOfflineMachineForCall(args, on) : null;
    if (offline) {
      const wakeRes = await onEnsureAwake(offline.machine, {
        kind: offline.kind,
        label: offline.label || `run ${tool.name}`,
        onContinue: () => executeCallDirect(args, on),
      });
      if (wakeRes?.awake || wakeRes?.cancelled || wakeRes?.forgotten) {
        return null;
      }
      // Machine cannot be woken or device lacks permission -> fall through to today's plain error.
    }
    return executeCallDirect(args, on);
  }

  async function executeCallDirect(args, on) {
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    setRunning(true);
    setError(null);
    setResult(null);
    setEvents([]);

    try {
      const out = await link.call(tool.name, args, {
        on,
        signal: ctrl.signal,
        onEvent(evt) {
          if (evt) setEvents((prev) => [...prev, evt]);
        },
      });
      setResult(out);
      if (folderChanges && !args?.dry_run && typeof on === 'string' && on !== 'all') {
        const machine = on;
        if ((tool.name === 'fs.mkdir' || tool.name === 'fs.remove' || tool.name === 'fs.write') && args?.path) {
          folderChanges.changed(machine, parentFilePath(args.path) || '~', { reason: tool.name });
        } else if ((tool.name === 'fs.move' || tool.name === 'fs.copy') && args?.to) {
          folderChanges.changed(machine, parentFilePath(args.to) || '~', { reason: tool.name });
          if (tool.name === 'fs.move' && args?.from) {
            folderChanges.changed(machine, parentFilePath(args.from) || '~', { reason: tool.name });
          }
        } else if (tool.name === 'fs.transfer' && args?.to && !out?.job?.id) {
          const dest = parseTransferTarget(args.to, machine);
          if (dest) folderChanges.changed(dest.machine, dest.dir, { reason: 'transfer' });
          if (args?.move && args?.from) {
            const src = parseTransferTarget(args.from, machine);
            if (src) folderChanges.changed(src.machine, src.dir, { reason: 'transfer' });
          }
        }
      }
      return out;
    } catch (err) {
      setError(err);
      return null;
    } finally {
      abortRef.current = null;
      setRunning(false);
    }
  }

  function handleRun(dryOnly) {
    setError(null);
    if (running && abortRef.current) {
      abortRef.current.abort();
      return;
    }
    const args = collectArgs();
    if (!args) return;

    const target = currentTarget();
    if (targetMode === 'all' && isEffectful(tool.effect) && target.machines.length === 0) {
      setError({
        code: 'unreachable',
        message: 'No answering machines are available to run this on.',
        hint: null,
      });
      return;
    }

    const toolHasDry = hasDryRun(tool.input_schema);
    if (dryOnly && toolHasDry) {
      executeCall({ ...args, dry_run: true }, target.on);
      return;
    }

    const effectiveDry = toolHasDry && Boolean(args.dry_run);
    const shouldConfirm =
      (needsConfirmation(tool.effect) || (targetMode === 'all' && isEffectful(tool.effect))) &&
      !effectiveDry;
    if (shouldConfirm) {
      const where = target.label;
      const frozenOn = Array.isArray(target.on) ? [...target.on] : target.on;
      const frozenMachines = new Set(target.machines);
      const targetJobs = (runningJobs || []).filter((j) => frozenMachines.has(j.machine));
      const matchedMachines = (machines || []).filter((m) => frozenMachines.has(m.name));
      const busyReasons = matchedMachines.flatMap((m) => m.busyReasons || []);
      const copy = describePowerConfirm(tool.name, where, args, targetJobs, busyReasons);
      onRequestConfirm({
        title: copy.title,
        summary: copy.summary,
        jobs: tool.effect === 'power' && tool.name !== 'power.wake' ? targetJobs : [],
        targetName: where,
        args,
        tool: tool.name,
        effect: tool.effect,
        confirmLabel: copy.confirmLabel,
        canDryRun: toolHasDry,
        async onDryRun() {
          return executeCall({ ...args, dry_run: true }, frozenOn);
        },
        async onConfirm() {
          await executeCall(args, frozenOn);
        },
      });
      return;
    }

    executeCall(args, target.on);
  }

  const toolHasDry = hasDryRun(tool.input_schema);
  const danger = needsConfirmation(tool.effect);
  const visibleFields = fields.filter((f) => !(danger && f.name === 'dry_run'));
  const where = currentTarget().label;
  const actionButtonLabel = danger
    ? describePowerConfirm(tool.name, where, values).buttonLabel
    : 'Run';
  const canSubmit = allowed || isSingleRemote;

  function toggleMulti(name) {
    setMultiTargets((prev) =>
      prev.includes(name) ? prev.filter((item) => item !== name) : [...prev, name],
    );
  }

  return html`
    <div>
      <p class="lede"><${CodeText} text=${descParts.first} /></p>
      ${descParts.rest
        ? html`
            <details class="more">
              <summary>More about this</summary>
              <p><${CodeText} text=${descParts.rest} /></p>
            </details>
          `
        : null}

      ${!allowed && !isSingleRemote
        ? html`
            <${Notice}
              kind="warn"
              title="Unavailable for this device."
              hint=${unavailableReason(tool, nodeInfo?.whoami)}
            />
          `
        : null}

      ${!hideTargetPicker
        ? html`
            <label class="field" htmlFor="tool-target-on">
              <span>on <em>where to run it</em></span>
              <select
                id="tool-target-on"
                value=${targetMode}
                onChange=${(e) => {
                  setTargetMode(e.target.value);
                  setError(null);
                  setResult(null);
                }}
              >
                <option value="local">
                  ${localNode === 'This computer' ? 'This computer' : `${localNode} (this machine)`}
                </option>
                ${remoteMachines.map((m) => {
                  const suffix =
                    m.status === 'asking' || m.asking
                      ? ' (checking…)'
                      : !m.reachable
                        ? ' (off)'
                        : '';
                  return html`<option key=${m.name} value=${m.name}>${m.name}${suffix}</option>`;
                })}
                ${machineNames.length > 1
                  ? html`
                      <option value="all">
                        ${skippedNames.length > 0
                          ? `All on machines (${answeringNames.length} of ${machineNames.length})`
                          : 'All machines'}
                      </option>
                    `
                  : null}
                ${machineNames.length > 1
                  ? html`<option value="multi">Choose several machines…</option>`
                  : null}
              </select>
              ${isSingleRemote
                ? html`
                    <small>
                      ${`Showing tools from ${localNode}; ${targetMode} checks its own tools and permissions when you run.`}
                    </small>
                  `
                : targetMode === 'all' && skippedNames.length > 0
                  ? html`
                      <small>
                        ${`Running on answering machines only; skipping ${skippedNames.join(', ')}.`}
                      </small>
                    `
                  : null}
            </label>
          `
        : null}

      ${!hideTargetPicker && targetMode === 'multi'
        ? html`
            <div class="chips" role="group" aria-label="Select target machines">
              ${machineList.map((m) => {
                const active = multiTargets.includes(m.name);
                const stateHint =
                  m.name === localNode || m.reachable
                    ? ''
                    : m.status === 'asking' || m.asking
                      ? ' · checking…'
                      : ' · off';
                return html`
                  <button
                    type="button"
                    key=${m.name}
                    class="chip-btn"
                    aria-pressed=${active ? 'true' : 'false'}
                    onClick=${() => toggleMulti(m.name)}
                  >
                    ${m.name}${stateHint}
                  </button>
                `;
              })}
            </div>
          `
        : null}

      ${technical
        ? html`
            <label class="switch">
              <span>Edit arguments as raw JSON</span>
              <input
                type="checkbox"
                checked=${rawMode}
                onChange=${(e) => toggleRaw(e.target.checked)}
              />
            </label>
          `
        : null}

      <form
        onSubmit=${(e) => {
          e.preventDefault();
          handleRun(false);
        }}
      >
        ${rawMode
          ? html`
              <label class="field" htmlFor="tool-raw-json">
                <span>arguments <em>JSON object</em></span>
                <textarea
                  id="tool-raw-json"
                  value=${rawText}
                  onInput=${(e) => setRawText(e.target.value)}
                  spellcheck="false"
                ></textarea>
              </label>
            `
          : visibleFields.map(
              (field) => html`
                <${FieldInput}
                  key=${field.name}
                  field=${field}
                  value=${values[field.name]}
                  error=${fieldErrors[field.name]}
                  idPrefix="arg"
                  onChange=${(nextVal) =>
                    setValues((prev) => ({ ...prev, [field.name]: nextVal }))}
                />
              `,
            )}

        ${error ? html`<${Notice} error=${error} />` : null}

        <div class="actions">
          ${danger && toolHasDry
            ? html`
                <button
                  type="button"
                  class="btn"
                  disabled=${!canSubmit || running}
                  onClick=${() => handleRun(true)}
                >
                  Try it (dry run)
                </button>
              `
            : null}
          <button
            type="submit"
            class=${running ? 'btn danger' : danger ? 'btn danger' : 'btn primary'}
            disabled=${!canSubmit && !running}
          >
            ${running ? 'Stop' : actionButtonLabel}
          </button>
        </div>
      </form>

      ${events.length > 0
        ? html`
            <p class="label">Events</p>
            <pre class="out wrap">
${events.map(
              (evt, i) =>
                html`<span key=${i} class=${evt.kind === 'stderr' ? 'err' : ''}>
${evt.machine ? `[${evt.machine}] ` : ''}${typeof evt.data === 'string'
                  ? evt.data
                  : JSON.stringify(evt.data) + '\n'}</span>`,
            )}</pre>
          `
        : null}

      ${result !== null ? html`<${ResultView} value=${result} />` : null}
    </div>
  `;
}

export function ToolsView({
  route,
  tools,
  allowedSet,
  nodeInfo,
  snapshot,
  link,
  folderChanges = null,
  wide,
  technical = true,
  onRequestConfirm,
  onEnsureAwake,
}) {
  const [query, setQuery] = useState('');
  const allowedList = useMemo(
    () => (allowedSet ? Array.from(allowedSet) : null),
    [allowedSet],
  );
  const groups = useMemo(
    () => groupToolsByFamily(tools || [], query, allowedList, nodeInfo?.whoami),
    [tools, query, allowedList, nodeInfo?.whoami],
  );

  const selectedName = wide ? route.tool || tools?.[0]?.name || null : route.tool || null;
  const selectedTool = (tools || []).find((t) => t.name === selectedName) || null;
  const selectedAllowed = selectedTool && allowedSet ? allowedSet.has(selectedTool.name) : true;

  const listPane = html`
    <section>
      <input
        class="input plain"
        type="search"
        value=${query}
        onInput=${(e) => setQuery(e.target.value)}
        placeholder="Search tools"
        aria-label="Search tools"
      />

      ${!tools || tools.length === 0
        ? html`<div class="empty">No tools available.</div>`
        : groups.length === 0
          ? html`<div class="empty">No tools match "${query}".</div>`
          : groups.map(
              (group) => html`
                <div key=${group.family}>
                  <p class="label">${group.family}</p>
                  <div class="list">
                    ${group.tools.map((tool) => {
                      const selected = wide && tool.name === selectedName;
                      const cls = [
                        'row',
                        !tool.allowed ? 'off' : '',
                        selected ? 'selected' : '',
                      ]
                        .filter(Boolean)
                        .join(' ');
                      return html`
                        <a
                          key=${tool.name}
                          class=${cls}
                          href=${routeHref({ section: 'actions', allTools: true, tool: tool.name })}
                          aria-current=${selected ? 'true' : undefined}
                        >
                          <span class="body">
                            <span class="title mono">
                              <span>${tool.name}</span>
                              <span class=${`chip fx ${tool.effect}`}>${tool.effect}</span>
                              ${!tool.allowed ? html`<span class="chip">unavailable</span>` : null}
                            </span>
                            <span class="sub">
                              ${tool.allowed
                                ? html`<${CodeText} text=${tool.summary} />`
                                : tool.reason}
                            </span>
                          </span>
                        </a>
                      `;
                    })}
                  </div>
                </div>
              `,
            )}
    </section>
  `;

  if (wide) {
    return html`
      <div class="split">
        <${ScrollContainer} as="section">
          ${listPane}
        <//>
        <${ScrollContainer} as="section">
          ${selectedTool
            ? html`
                <h2 class="pane-title">
                  <span>${selectedTool.name}</span>
                  <span class=${`chip fx ${selectedTool.effect}`}>${selectedTool.effect}</span>
                </h2>
                <${ToolDetail}
                  key=${selectedTool.name}
                  tool=${selectedTool}
                  allowed=${selectedAllowed}
                  nodeInfo=${nodeInfo}
                  machines=${snapshot?.machines || []}
                  runningJobs=${snapshot?.runningJobs || []}
                  link=${link}
                  folderChanges=${folderChanges}
                  technical=${technical}
                  onRequestConfirm=${onRequestConfirm}
                  onEnsureAwake=${onEnsureAwake}
                />
              `
            : html`<div class="empty">Select a tool to run it.</div>`}
        <//>
      </div>
    `;
  }

  if (route.tool && selectedTool) {
    return html`
      <${ToolDetail}
        key=${selectedTool.name}
        tool=${selectedTool}
        allowed=${selectedAllowed}
        nodeInfo=${nodeInfo}
        machines=${snapshot?.machines || []}
        runningJobs=${snapshot?.runningJobs || []}
        link=${link}
        folderChanges=${folderChanges}
        technical=${technical}
        onRequestConfirm=${onRequestConfirm}
        onEnsureAwake=${onEnsureAwake}
      />
    `;
  }

  return listPane;
}
