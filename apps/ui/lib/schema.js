// Pure JSON Schema form helpers and tool catalog grouping.

import { firstSentence, formatBusyReasonLines } from './format.js';

function baseType(prop) {
  if (!prop || typeof prop !== 'object') return 'json';
  if (Array.isArray(prop.enum) && prop.enum.length > 0) return 'enum';
  let type = prop.type;
  if (Array.isArray(type)) {
    type = type.find((t) => t !== 'null') || type[0];
  }
  if (type === 'string') return 'string';
  if (type === 'integer') return 'integer';
  if (type === 'number') return 'number';
  if (type === 'boolean') return 'boolean';
  if (type === 'array') {
    const itemType = prop.items?.type;
    if (itemType === 'string') return 'string-array';
    return 'json';
  }
  if (type === 'object') {
    if (prop.properties && Object.keys(prop.properties).length > 0) {
      return 'object';
    }
    return 'json';
  }
  return 'json';
}

function describeField(name, prop, requiredSet, allowNested) {
  const kind = baseType(prop);
  const actualKind = kind === 'object' && !allowNested ? 'json' : kind;
  const field = {
    name,
    kind: actualKind,
    required: requiredSet.has(name),
    description: prop?.description ? String(prop.description).replace(/\s+/g, ' ').trim() : '',
    defaultValue: Object.prototype.hasOwnProperty.call(prop || {}, 'default') ? prop.default : undefined,
    enumValues: actualKind === 'enum' ? prop.enum : null,
    minimum: typeof prop?.minimum === 'number' ? prop.minimum : null,
    maximum: typeof prop?.maximum === 'number' ? prop.maximum : null,
    fields: null,
  };
  if (actualKind === 'object') {
    const subReq = new Set(Array.isArray(prop.required) ? prop.required : []);
    field.fields = Object.entries(prop.properties).map(([subName, subProp]) =>
      describeField(subName, subProp, subReq, false),
    );
  }
  return field;
}

export function describeSchema(schema) {
  if (!schema || typeof schema !== 'object' || !schema.properties) return [];
  const requiredSet = new Set(Array.isArray(schema.required) ? schema.required : []);
  return Object.entries(schema.properties).map(([name, prop]) =>
    describeField(name, prop, requiredSet, true),
  );
}

export function initialValues(schema) {
  const fields = describeSchema(schema);
  const values = {};
  for (const field of fields) {
    if (field.kind === 'boolean') {
      values[field.name] = Boolean(field.defaultValue);
    } else if (field.kind === 'object') {
      const nested = {};
      for (const sub of field.fields) {
        nested[sub.name] = sub.kind === 'boolean' ? Boolean(sub.defaultValue) : '';
      }
      values[field.name] = nested;
    } else {
      values[field.name] = '';
    }
  }
  return values;
}

function coerceField(field, raw, errors, prefix = '') {
  const label = prefix ? `${prefix}.${field.name}` : field.name;

  if (field.kind === 'boolean') {
    if (typeof raw === 'boolean') {
      if (field.defaultValue !== undefined && raw === field.defaultValue && !field.required) {
        return { include: false };
      }
      return { include: raw === true || field.required || field.defaultValue === true, value: raw };
    }
    return { include: false };
  }

  if (field.kind === 'object') {
    const obj = raw && typeof raw === 'object' ? raw : {};
    const out = {};
    for (const sub of field.fields) {
      const coerced = coerceField(sub, obj[sub.name], errors, label);
      if (coerced.include) out[sub.name] = coerced.value;
    }
    if (Object.keys(out).length === 0) {
      if (field.required) errors[label] = `${label} is required.`;
      return { include: false };
    }
    return { include: true, value: out };
  }

  const text = raw === undefined || raw === null ? '' : String(raw);
  const trimmed = text.trim();

  if (trimmed === '') {
    if (field.required) errors[label] = `${label} is required.`;
    return { include: false };
  }

  if (field.kind === 'string' || field.kind === 'enum') {
    return { include: true, value: field.kind === 'enum' ? trimmed : text };
  }

  if (field.kind === 'integer') {
    const n = Number(trimmed);
    if (!Number.isInteger(n)) {
      errors[label] = `${label} must be a whole number.`;
      return { include: false };
    }
    if (field.minimum !== null && n < field.minimum) {
      errors[label] = `${label} must be at least ${field.minimum}.`;
      return { include: false };
    }
    if (field.maximum !== null && n > field.maximum) {
      errors[label] = `${label} must be at most ${field.maximum}.`;
      return { include: false };
    }
    return { include: true, value: n };
  }

  if (field.kind === 'number') {
    const n = Number(trimmed);
    if (!Number.isFinite(n)) {
      errors[label] = `${label} must be a number.`;
      return { include: false };
    }
    if (field.minimum !== null && n < field.minimum) {
      errors[label] = `${label} must be at least ${field.minimum}.`;
      return { include: false };
    }
    if (field.maximum !== null && n > field.maximum) {
      errors[label] = `${label} must be at most ${field.maximum}.`;
      return { include: false };
    }
    return { include: true, value: n };
  }

  if (field.kind === 'string-array') {
    const items = text
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter(Boolean);
    if (items.length === 0) {
      if (field.required) errors[label] = `${label} is required.`;
      return { include: false };
    }
    return { include: true, value: items };
  }

  // Fallback JSON field
  try {
    return { include: true, value: JSON.parse(trimmed) };
  } catch (err) {
    errors[label] = `${label} must be valid JSON: ${err.message}`;
    return { include: false };
  }
}

export function buildArgs(schema, values) {
  const fields = describeSchema(schema);
  const args = {};
  const errors = {};
  for (const field of fields) {
    const coerced = coerceField(field, values?.[field.name], errors);
    if (coerced.include) {
      args[field.name] = coerced.value;
    }
  }
  return { ok: Object.keys(errors).length === 0, args, errors };
}

export function valuesFromArgs(schema, args) {
  const fields = describeSchema(schema);
  const values = initialValues(schema);
  if (!args || typeof args !== 'object') return values;
  for (const field of fields) {
    if (!Object.prototype.hasOwnProperty.call(args, field.name)) continue;
    const val = args[field.name];
    if (field.kind === 'boolean') {
      values[field.name] = Boolean(val);
    } else if (field.kind === 'string-array') {
      values[field.name] = Array.isArray(val) ? val.join('\n') : String(val ?? '');
    } else if (field.kind === 'object' && val && typeof val === 'object') {
      const nested = { ...values[field.name] };
      for (const sub of field.fields) {
        if (!Object.prototype.hasOwnProperty.call(val, sub.name)) continue;
        nested[sub.name] =
          sub.kind === 'boolean'
            ? Boolean(val[sub.name])
            : sub.kind === 'json'
              ? JSON.stringify(val[sub.name], null, 2)
              : String(val[sub.name] ?? '');
      }
      values[field.name] = nested;
    } else if (field.kind === 'json') {
      values[field.name] = JSON.stringify(val, null, 2);
    } else {
      values[field.name] = val === undefined || val === null ? '' : String(val);
    }
  }
  return values;
}

export function parseRawArgs(text) {
  const trimmed = String(text ?? '').trim();
  if (!trimmed) return { ok: true, args: {}, error: null };
  try {
    const parsed = JSON.parse(trimmed);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return { ok: false, args: null, error: 'Arguments must be a JSON object.' };
    }
    return { ok: true, args: parsed, error: null };
  } catch (err) {
    return { ok: false, args: null, error: `Invalid JSON: ${err.message}` };
  }
}

export function hasDryRun(schema) {
  return Boolean(schema?.properties?.dry_run && schema.properties.dry_run.type === 'boolean');
}

export function needsConfirmation(effect) {
  return effect === 'destroy' || effect === 'power';
}

export function unavailableReason(tool, whoami) {
  if (!whoami) return `Your current role does not allow ${tool.name}.`;
  const role = whoami.role || 'unknown';
  const principal = whoami.principal || 'unknown';
  return `Role "${role}" (${principal}) does not allow ${tool.name} (${tool.effect}).`;
}

const ALL_EFFECTS = ['read', 'write', 'exec', 'destroy', 'power', 'admin'];

const MISSING_EFFECT_PHRASES = {
  read: 'read machine state (read)',
  write: 'change files (write)',
  exec: 'run commands (exec)',
  destroy: 'remove files or stop processes (destroy)',
  power: 'change power state (power)',
  admin: 'change settings or permissions (admin)',
};

function joinList(items, conj = 'or') {
  if (items.length === 0) return '';
  if (items.length === 1) return items[0];
  if (items.length === 2) return `${items[0]} ${conj} ${items[1]}`;
  return `${items.slice(0, -1).join(', ')} ${conj} ${items[items.length - 1]}`;
}

export function describeRoleAccess(whoami, tools) {
  if (!whoami) return ['Permissions are decided by the node policy.'];
  const allowed = new Set(whoami.tools || []);
  const presentEffects = new Set();
  const allowedEffects = new Set();
  for (const t of tools || []) {
    if (t.effect) presentEffects.add(t.effect);
    if (allowed.has(t.name) && t.effect) allowedEffects.add(t.effect);
  }
  const orderedAllowed = ALL_EFFECTS.filter((e) => allowedEffects.has(e));
  const missingPool = presentEffects.size > 0 ? ALL_EFFECTS.filter((e) => presentEffects.has(e)) : ALL_EFFECTS;
  const missing = missingPool.filter((e) => !allowedEffects.has(e));

  const lines = [];
  if (orderedAllowed.length > 0) {
    lines.push(`May run ${allowed.size} tools (${orderedAllowed.join(', ')}).`);
  } else {
    lines.push('May not run any tools.');
  }
  if (missing.length > 0 && orderedAllowed.length > 0) {
    const phrases = missing.map((e) => MISSING_EFFECT_PHRASES[e] || e);
    lines.push(`May not ${joinList(phrases, 'or')}.`);
  }
  if (Array.isArray(whoami.paths) && whoami.paths.length > 0) {
    lines.push(`File tools are confined to ${whoami.paths.join(', ')}.`);
  } else if (whoami.paths === null) {
    lines.push('File tools have no extra path limit.');
  }
  if (whoami.delegate) {
    lines.push('May act as another principal.');
  }
  return lines;
}

export function describePowerConfirm(toolName, target, args = {}, runningJobs = [], busyReasons = []) {
  const machine = args?.machine || target || 'this machine';
  const jobs = Array.isArray(runningJobs) ? runningJobs : [];
  const reasonLines = formatBusyReasonLines(busyReasons);

  function interruptionSummary() {
    if (jobs.length === 1) return 'One job is running on it and will be interrupted.';
    if (jobs.length > 1) return `${jobs.length} jobs are running on it and will be interrupted.`;
    if (reasonLines.length > 0) return `It is busy right now: ${reasonLines.join('; ')}.`;
    return 'Nothing is running on it.';
  }

  switch (toolName) {
    case 'power.restart':
      return args?.into
        ? {
            title: `Restart ${target} into ${args.into}?`,
            confirmLabel: `Restart into ${args.into}`,
            buttonLabel: `Restart ${target}`,
            summary: interruptionSummary(),
          }
        : {
            title: `Restart ${target} now?`,
            confirmLabel: jobs.length > 0 ? 'Restart anyway' : `Restart ${target}`,
            buttonLabel: `Restart ${target}`,
            summary: interruptionSummary(),
          };
    case 'power.shutdown':
      return {
        title: `Shut down ${target} now?`,
        confirmLabel: jobs.length > 0 ? 'Shut down anyway' : `Shut down ${target}`,
        buttonLabel: `Shut down ${target}`,
        summary: interruptionSummary(),
      };
    case 'power.sleep':
      return {
        title: `Put ${target} to sleep now?`,
        confirmLabel: jobs.length > 0 ? 'Sleep anyway' : 'Sleep',
        buttonLabel: `Sleep ${target}`,
        summary: interruptionSummary(),
      };
    case 'power.wake':
      return {
        title: `Wake ${machine}?`,
        confirmLabel: 'Wake',
        buttonLabel: `Wake ${machine}`,
        summary: `Send a wake signal to ${machine}.`,
      };
    case 'power.switch':
      return {
        title: args?.into ? `Switch ${machine} to ${args.into}?` : `Switch ${machine}?`,
        confirmLabel: args?.into ? `Switch to ${args.into}` : `Switch ${machine}`,
        buttonLabel: args?.into ? `Switch ${machine} to ${args.into}` : `Switch ${machine}`,
        summary: interruptionSummary(),
      };
    case 'power.cancel':
      return {
        title: `Cancel pending power action on ${target}?`,
        confirmLabel: 'Cancel action',
        buttonLabel: 'Cancel pending action',
        summary: `Stops any sleep, restart, or shutdown scheduled on ${target}.`,
      };
    default:
      return {
        title: `Run ${toolName} on ${target}?`,
        confirmLabel: `Run ${toolName}`,
        buttonLabel: `Run ${toolName}`,
        summary: `This action will run on ${target}.`,
      };
  }
}

export function groupToolsByFamily(tools, query, allowedNames, whoami) {
  const needle = String(query || '').trim().toLowerCase();
  const allowedSet = allowedNames ? new Set(allowedNames) : null;
  const byFamily = new Map();

  for (const tool of tools || []) {
    const summary = firstSentence(tool.description);
    if (needle) {
      const hay = `${tool.name} ${tool.effect} ${tool.description}`.toLowerCase();
      if (!hay.includes(needle)) continue;
    }
    const family = tool.name.includes('.') ? tool.name.split('.')[0] : 'other';
    const allowed = allowedSet ? allowedSet.has(tool.name) : true;
    const item = {
      ...tool,
      family,
      summary,
      allowed,
      reason: allowed ? null : unavailableReason(tool, whoami),
    };
    if (!byFamily.has(family)) byFamily.set(family, []);
    byFamily.get(family).push(item);
  }

  return Array.from(byFamily.entries())
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([family, items]) => ({
      family,
      tools: items.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)),
    }));
}
