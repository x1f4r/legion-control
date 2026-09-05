// Administrative editing of machine-local service configuration. Restricted
// peers never receive this document: its argv may contain private credentials.
// Validation and saving configure existing adapters; neither runs commands.
import crypto from 'node:crypto';
import fs from 'node:fs';
import { configPath, normalizeConfig, readJsonDocument, saveConfig } from './config.mjs';
import { acquireOperationLock } from './lock.mjs';
import { REASON } from './contract.mjs';
import { discoverServiceProfiles } from './service-profiles.mjs';

export const MAX_SERVICE_CONFIG_BYTES = 1024 * 1024;
const fail = (reasonCode, message, extra = {}) => ({ ok: false, reasonCode, message, ...extra });
const plain = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (plain(value)) return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}

/** An opaque structural identity, so formatting alone does not cause conflict. */
export function serviceConfigHash(document) {
  return crypto.createHash('sha256').update(canonical(document)).digest('hex');
}

export function serviceTemplates() {
  const common = { id: '', name: '', health: { type: 'none' }, updates: { automatic: false } };
  return [
    { id: 'command', name: 'Command service', description: 'Supply version, update and verification commands. Choose a busy probe, or explicitly declare busy.type none.',
      service: { ...common, kind: 'command', installedVersion: [], latestVersion: [], update: [], verify: [], process: { type: 'none' } } },
    { id: 'systemd-user', name: 'Systemd user service', description: 'Reference an existing user unit and supply its update and verification commands. Choose a busy probe or explicitly declare none.',
      service: { ...common, kind: 'command', update: [], verify: [], process: { type: 'systemd-user', unit: '' } } },
    { id: 'npm', name: 'npm service', description: 'Choose a package and its process adapter. Supply a busy probe or explicitly declare none. No package is installed when saving.',
      service: { ...common, kind: 'npm', package: '', channel: 'latest', allowScripts: [], process: { type: 'none' } } },
  ];
}

function currentDocument() {
  const file = configPath();
  try {
    if (fs.existsSync(file) && fs.statSync(file).size > MAX_SERVICE_CONFIG_BYTES) return null;
    const document = readJsonDocument(file);
    if (document.state === 'missing') return {};
    return document.state === 'ok' && plain(document.value) ? document.value : null;
  } catch { return null; }
}

// Schema diagnostics may quote rejected argv or values. Keep only field paths
// here, never raw diagnostic prose, to avoid echoing credentials in a failure.
function issues(entries = []) {
  return entries.map((entry) => ({
    path: typeof entry.path === 'string' && /^[A-Za-z0-9_.[\]-]{0,200}$/.test(entry.path) ? entry.path : 'document',
    message: 'Check this field against the supported configuration schema.',
  }));
}

function changes(before, after) {
  const oldServices = new Map((Array.isArray(before.services) ? before.services : []).map((service) => [service.id, service]));
  const newServices = new Map((Array.isArray(after.services) ? after.services : []).map((service) => [service.id, service]));
  return {
    added: [...newServices.keys()].filter((id) => !oldServices.has(id)),
    removed: [...oldServices.keys()].filter((id) => !newServices.has(id)),
    changed: [...newServices.keys()].filter((id) => oldServices.has(id) && canonical(oldServices.get(id)) !== canonical(newServices.get(id))),
    otherSettingsChanged: canonical({ ...before, services: [] }) !== canonical({ ...after, services: [] }),
  };
}

function argumentIssues(document) {
  const argvFields = new Set(['command', 'start', 'stop', 'running', 'installedVersion', 'latestVersion', 'update', 'verify', 'rollback', 'arm', 'reboot', 'rebootHelper']);
  const errors = [];
  const visit = (value, where = 'document', depth = 0) => {
    if (depth > 64) { errors.push({ path: 'document', message: 'Configuration nesting is too deep.' }); return; }
    if (Array.isArray(value)) {
      for (const [index, item] of value.entries()) visit(item, `${where}[${index}]`, depth + 1);
    } else if (plain(value)) {
      for (const [key, item] of Object.entries(value)) {
        if (argvFields.has(key) && Array.isArray(item) && item.some((argument) => typeof argument !== 'string' || argument.length === 0 || argument.includes('\0'))) {
          errors.push({ path: `${where}.${key}`, message: 'Every command argument must be a nonempty string without NUL characters.' });
        }
        visit(item, `${where}.${key}`, depth + 1);
      }
    }
  };
  visit(document);
  return errors;
}

function validateRequest(request, before) {
  if (!plain(request) || Object.keys(request).some((key) => !['expectedHash', 'document'].includes(key)) || !/^[0-9a-f]{64}$/.test(request.expectedHash ?? '') || !plain(request.document)) {
    return fail(REASON.badArgument, 'Provide exactly expectedHash from service-config get and a document object.', { valid: false });
  }
  const hash = serviceConfigHash(before);
  if (hash !== request.expectedHash) return fail(REASON.staleRevision, 'Agent configuration changed; reload and review your edits before saving.', { valid: false, conflict: true, hash });
  if (request.document.configVersion !== 3 || !Array.isArray(request.document.services)) {
    return fail(REASON.configInvalid, 'The edited document requires configVersion 3 and an explicit services array.', { valid: false });
  }
  const strictErrors = argumentIssues(request.document);
  if (strictErrors.length) return fail(REASON.configInvalid, 'The configuration contains invalid command arguments or nesting; nothing was saved.', { valid: false, errors: strictErrors });
  const normalized = normalizeConfig(request.document, { present: true });
  const errors = issues(normalized.errors);
  for (const [index, service] of request.document.services.entries()) {
    if (!plain(service?.busy) || typeof service.busy.type !== 'string') {
      errors.push({ path: `services[${index}].busy`, message: 'Choose a busy probe, or explicitly set type none to allow work without busy protection.' });
    }
  }
  if (errors.length) return fail(REASON.configInvalid, 'The configuration is incomplete or invalid; nothing was saved.', { valid: false, errors });
  return { ok: true, valid: true, hash, proposedHash: serviceConfigHash(request.document), changes: changes(before, request.document), warnings: issues(normalized.warnings) };
}

/** Read only for an unrestricted administrative session. */
export function getServiceConfig() {
  if (process.env.LEGIONCTL_RESTRICTED === '1') return fail(REASON.restricted, 'Service provisioning requires an unrestricted administrative session.');
  const stored = currentDocument();
  if (!stored) return fail(REASON.configInvalid, 'Agent configuration cannot be read as a bounded JSON object; repair the file before using the editor.');
  return {
    ok: true, hash: serviceConfigHash(stored),
    document: Object.keys(stored).length ? (stored.configVersion === undefined ? { ...stored, configVersion: 3 } : stored) : { configVersion: 3, services: [], updates: { automatic: false }, boot: { targets: {} } },
    templates: serviceTemplates(), profiles: discoverServiceProfiles({ configuration: stored }), limits: { maxBytes: MAX_SERVICE_CONFIG_BYTES },
  };
}

export function validateServiceConfig(request) {
  if (process.env.LEGIONCTL_RESTRICTED === '1') return fail(REASON.restricted, 'Service provisioning requires an unrestricted administrative session.');
  const before = currentDocument();
  if (!before) return fail(REASON.configInvalid, 'Agent configuration cannot be read; nothing was saved.');
  return validateRequest(request, before);
}

export function setServiceConfig(request) {
  if (process.env.LEGIONCTL_RESTRICTED === '1') return fail(REASON.restricted, 'Service provisioning requires an unrestricted administrative session.');
  const first = validateServiceConfig(request);
  if (!first.ok) return first;
  const taken = acquireOperationLock({ kind: 'service-config', waitMs: 250 });
  if (!taken.ok) return fail(taken.reasonCode ?? REASON.internal, 'Another operation is running or its mutex is unavailable; nothing was saved.');
  try {
    let checked;
    const saved = saveConfig((latest) => {
      checked = validateRequest(request, latest);
      if (!checked.ok) throw new Error('configuration comparison or validation failed');
      return request.document;
    });
    if (checked && !checked.ok) return checked;
    if (!saved.ok) return fail(REASON.internal, 'The configuration could not be saved; retry after checking permissions and active operations.');
    return { ...checked, hash: checked.proposedHash, saved: true, message: 'Agent configuration saved. No setup or service command was executed.' };
  } finally { taken.lock.release(); }
}

/** Bounded input, entirely read before the operation mutex is acquired. */
export function readServiceConfigInput(stream = process.stdin, { idleMs = 5000, totalMs = 15000 } = {}) {
  return new Promise((resolve) => {
    const chunks = [];
    let size = 0;
    let finished = false;
    let idle;
    let total;
    const finish = (value, close = false) => {
      if (finished) return;
      finished = true;
      clearTimeout(idle); clearTimeout(total);
      stream.removeListener('data', onData); stream.removeListener('end', onEnd); stream.removeListener('error', onError);
      if (close) stream.destroy();
      resolve(value);
    };
    const timeout = () => finish(fail(REASON.timedOut, 'Configuration input timed out; nothing was saved.'), true);
    const onData = (chunk) => {
      size += Buffer.byteLength(chunk);
      if (size > MAX_SERVICE_CONFIG_BYTES) return finish(fail(REASON.badArgument, 'Configuration input exceeds the size limit; nothing was saved.'), true);
      chunks.push(Buffer.from(chunk));
      clearTimeout(idle); idle = setTimeout(timeout, idleMs);
    };
    const onEnd = () => {
      try {
        const text = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks));
        finish({ ok: true, request: JSON.parse(text) });
      } catch { finish(fail(REASON.badArgument, 'Configuration input must be valid UTF-8 JSON; nothing was saved.')); }
    };
    const onError = () => finish(fail(REASON.badArgument, 'Configuration input could not be read; nothing was saved.'));
    stream.on('data', onData); stream.once('end', onEnd); stream.once('error', onError);
    idle = setTimeout(timeout, idleMs); total = setTimeout(timeout, totalMs);
  });
}

export async function commandServiceConfig(positional, flags) {
  if (process.env.LEGIONCTL_RESTRICTED === '1') return fail(REASON.restricted, 'Service provisioning requires an unrestricted administrative session.');
  const verb = positional[0];
  if (positional.length !== 1 || !['get', 'validate', 'set'].includes(verb) || (verb === 'get' ? flags.size !== 0 : !flags.has('stdin') || flags.size !== 1)) {
    return fail(REASON.badArgument, 'Use service-config get, service-config validate --stdin, or service-config set --stdin.');
  }
  if (verb === 'get') return getServiceConfig();
  const input = await readServiceConfigInput();
  if (!input.ok) return input;
  return verb === 'set' ? setServiceConfig(input.request) : validateServiceConfig(input.request);
}
