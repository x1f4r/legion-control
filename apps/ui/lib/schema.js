// Pure JSON Schema form helpers and tool catalog grouping.

import { firstSentence } from './format.js';

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
