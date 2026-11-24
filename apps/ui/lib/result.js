// Pure helpers for inspecting tool results and fan-out outcomes.

export function isFanOut(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  if (keys.length !== 1 || keys[0] !== 'results') return false;
  const results = value.results;
  if (!results || typeof results !== 'object' || Array.isArray(results)) return false;
  const entries = Object.values(results);
  if (entries.length === 0) return true;
  return entries.every(
    (entry) =>
      entry &&
      typeof entry === 'object' &&
      !Array.isArray(entry) &&
      (Object.prototype.hasOwnProperty.call(entry, 'ok') ||
        Object.prototype.hasOwnProperty.call(entry, 'error')),
  );
}

export function fanOutEntries(value) {
  if (!isFanOut(value)) return [];
  return Object.entries(value.results)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([machine, entry]) => {
      if (Object.prototype.hasOwnProperty.call(entry, 'ok')) {
        return { machine, ok: true, value: entry.ok, error: null };
      }
      return { machine, ok: false, value: null, error: entry.error };
    });
}

export function formatScalar(value) {
  if (value === null) return 'null';
  if (value === undefined) return '';
  if (typeof value === 'boolean') return value ? 'yes' : 'no';
  if (typeof value === 'number') return String(value);
  if (typeof value === 'string') return value;
  return JSON.stringify(value);
}

export function classifyResult(value) {
  if (value === null || typeof value !== 'object') {
    return { kind: 'scalar', text: formatScalar(value) };
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return { kind: 'empty-array' };
    const allScalars = value.every((item) => item === null || typeof item !== 'object');
    if (allScalars) {
      return { kind: 'scalar-array', items: value.map(formatScalar) };
    }
    return { kind: 'object-array', items: value.map(classifyResult) };
  }
  const entries = Object.entries(value);
  if (entries.length === 0) return { kind: 'empty-object' };
  return {
    kind: 'object',
    entries: entries.map(([key, val]) => ({ key, node: classifyResult(val) })),
  };
}
