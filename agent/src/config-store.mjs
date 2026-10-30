// Reading and writing config.json, and what a missing or broken one means.
//
// THE HANDLING RULE, and it is the opposite of what the first version did:
//
//   No file at all is INERT. Present but wrong is FATAL.
//
// A missing config.json used to mean "the documented defaults", and those
// defaults described a specific product, with automatic updates on. So a machine
// that had never been configured, or whose config had been moved, would happily
// decide it looked after a T3 Code install and start updating it. A machine with
// no configuration now looks after nothing and has automatic updates off, which
// is the only honest reading of "nobody has told me anything".
//
// A config.json that is truncated, unreadable or contains a key the agent does
// not understand is something else again: it is a machine whose owner tried to
// say something and was not heard. That case refuses to do anything, keeps the
// last copy that loaded cleanly for comparison, and says what to fix.

import fs from 'node:fs';
import { normalizeConfig } from './config-schema.mjs';
import { configPath, lastGoodConfigPath } from './paths.mjs';
import { acquireFileLock, stateLockPath, writeJsonAtomic, writePrivateAtomic } from './state.mjs';

function isPlainObject(value) {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

/**
 * Read a JSON document, telling the cases apart.
 *
 * Collapsing "there is no file", "the file cannot be read" and "the file is not
 * JSON" into one empty object is the specific bug this replaces: a truncated
 * config used to load as the full set of enabled defaults.
 */
export function readJsonDocument(file) {
  let raw;
  try {
    raw = fs.readFileSync(file, 'utf8');
  } catch (err) {
    if (err.code === 'ENOENT') return { state: 'missing', value: null, error: null };
    return { state: 'unreadable', value: null, error: `${file} could not be read: ${err.message}` };
  }
  if (raw.trim().length === 0) return { state: 'empty', value: null, error: `${file} is empty`, raw };
  try {
    const parsed = JSON.parse(raw);
    return { state: 'ok', value: parsed, error: null, raw };
  } catch (err) {
    return { state: 'malformed', value: null, error: `${file} does not parse as JSON: ${jsonParseProblem(err, raw)}`, raw };
  }
}

/**
 * Where JSON text stops parsing, said without quoting it: "line 3, column 8",
 * "the JSON ends early", or "invalid JSON".
 *
 * V8's own message quotes the text around the fault, and the text is
 * config.json, whose arguments a restricted key must not read (contract
 * section 13). Its position is taken from the message when it gives one, and
 * found here when it does not: V8 names no position for an unexpected token,
 * which is the commonest hand edit there is, a trailing comma or a comment.
 */
export function jsonParseProblem(error, raw) {
  const message = String(error?.message ?? '');
  const reported = /\(line ([0-9]+) column ([0-9]+)\)/.exec(message);
  if (reported) return `line ${reported[1]}, column ${reported[2]}`;
  const text = typeof raw === 'string' ? raw : '';
  const position = /at position ([0-9]+)/.exec(message);
  const offset = position ? Number(position[1]) : jsonStopOffset(text);
  if (offset !== null && offset < text.length) {
    const before = text.slice(0, offset);
    const line = before.split('\n').length;
    return `line ${line}, column ${offset - before.lastIndexOf('\n')}`;
  }
  if (offset !== null || /Unexpected end of JSON input/.test(message)) return 'the JSON ends early';
  return 'invalid JSON';
}

const JSON_NUMBER = /-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/y;

/**
 * Where a strict JSON reader stops in `text`: the offset of the first
 * character it cannot take, the length of the text when the text ends too
 * early, or null when it is JSON after all.
 */
function jsonStopOffset(text) {
  let at = 0;
  const stop = () => { throw at; };
  const space = () => { while (at < text.length && ' \t\n\r'.includes(text[at])) at += 1; };
  const expect = (character) => {
    if (text[at] !== character) stop();
    at += 1;
  };
  const string = () => {
    expect('"');
    while (text[at] !== '"') {
      if (at >= text.length || text.charCodeAt(at) < 0x20) stop();
      if (text[at] !== '\\') {
        at += 1;
        continue;
      }
      at += 1;
      if (text[at] === 'u') {
        at += 1;
        for (let digit = 0; digit < 4; digit += 1) {
          if (!/[0-9A-Fa-f]/.test(text[at] ?? '')) stop();
          at += 1;
        }
      } else if (at < text.length && '"\\/bfnrt'.includes(text[at])) {
        at += 1;
      } else {
        stop();
      }
    }
    at += 1;
  };
  const value = () => {
    space();
    if (text[at] === '{' || text[at] === '[') {
      const close = text[at] === '{' ? '}' : ']';
      at += 1;
      space();
      if (text[at] === close) {
        at += 1;
        return;
      }
      for (;;) {
        if (close === '}') {
          space();
          string();
          space();
          expect(':');
        }
        value();
        space();
        if (text[at] !== ',') break;
        at += 1;
      }
      expect(close);
      return;
    }
    if (text[at] === '"') return string();
    for (const literal of ['true', 'false', 'null']) {
      if (text[at] === literal[0]) {
        for (const character of literal) expect(character);
        return;
      }
    }
    JSON_NUMBER.lastIndex = at;
    const number = JSON_NUMBER.exec(text);
    if (number === null) stop();
    at += number[0].length;
  };
  try {
    value();
    space();
    return at < text.length ? at : null;
  } catch (offset) {
    // A document nested deeper than the stack is not one to locate by hand.
    return typeof offset === 'number' ? offset : null;
  }
}

/** Keep a copy of the last config that loaded cleanly, for doctor to compare against. */
function rememberLastGood(raw) {
  if (typeof raw !== 'string') return;
  try {
    const file = lastGoodConfigPath();
    if (fs.existsSync(file) && fs.readFileSync(file, 'utf8') === raw) return;
    writePrivateAtomic(file, raw);
  } catch {
    /* the last-good copy is a convenience, never a requirement */
  }
}

/** When the last-good copy was written, or null when there is none. */
export function lastGoodInfo() {
  try {
    const stat = fs.statSync(lastGoodConfigPath());
    return { path: lastGoodConfigPath(), at: new Date(stat.mtimeMs).toISOString() };
  } catch {
    return null;
  }
}

/**
 * config.json, normalized, with everything that is wrong with it.
 *
 * Returns { ok, config, source, problems, errors, warnings, migrations }.
 *
 *   source "defaults"        no config.json; the machine is inert.
 *   source "file"            config.json was read.
 *   source "last-known-good" the file is broken and an earlier good copy exists;
 *                            it is REPORTED, not applied. ok stays false and
 *                            every mutation is refused, because quietly running
 *                            yesterday's policy is not what the owner asked for
 *                            either.
 */
export function loadConfig(options = {}) {
  const file = options.path ?? configPath();
  // `status`, `doctor`, `op`, `history` and `logs` promise to change nothing on
  // the machine. Keeping the last-good copy up to date is maintenance, and
  // maintenance belongs to the commands that were asked to change something.
  const readOnly = options.readOnly === true;
  const document = readJsonDocument(file);

  if (document.state === 'missing') {
    const result = normalizeConfig({}, { ...options, present: false });
    return {
      ...result,
      source: 'defaults',
      path: file,
      error: null,
      problems: result.errors,
      warnings: [
        ...result.warnings,
        {
          level: 'warning',
          path: 'config.json',
          message: `there is no ${file}, so this machine looks after nothing and automatic updates are off`,
          fix: 'write a config.json to tell the agent what to look after; "legionctl doctor" lists what it would check',
        },
      ],
    };
  }

  if (document.state !== 'ok') {
    const lastGood = lastGoodInfo();
    const problem = {
      level: 'error',
      path: 'config.json',
      message: document.error,
      fix: lastGood
        ? `the last copy that loaded cleanly is at ${lastGood.path} (${lastGood.at}); compare it with "legionctl doctor" and copy it back if it is still right`
        : 'fix or remove the file; the agent will not guess at settings, and it will not fall back to defaults that might enable automatic updates you had turned off',
    };
    return {
      ok: false,
      config: null,
      source: lastGood ? 'last-known-good' : 'defaults',
      path: file,
      lastGood,
      errors: [problem],
      problems: [problem],
      warnings: [],
      migrations: [],
      error: document.error,
    };
  }

  const result = normalizeConfig(document.value, { ...options, present: true });
  if (result.ok && !readOnly) rememberLastGood(document.raw);
  return {
    ...result,
    source: 'file',
    path: file,
    problems: result.errors,
    error: result.ok ? null : `${file} has ${result.errors.length} problem${result.errors.length === 1 ? '' : 's'}`,
  };
}

/**
 * Merge a patch into config.json and return the normalized result.
 *
 * Refuses outright when the stored document cannot be read. Writing a patch on
 * top of a file we could not parse would rewrite it as the patch plus nothing,
 * silently discarding whatever the owner had configured.
 *
 * Only the keys that were already in the file plus the patch are written back.
 * Stamping the full set of defaults would freeze today's defaults into every
 * config the first time anyone toggled a switch.
 */
export function saveConfig(patch, { waitMs = 10000 } = {}) {
  // A unique temporary filename only makes the final rename atomic. The read,
  // patch and validation must share the same lock, or two acknowledged changes
  // can each replace the other's snapshot. Callers that hold the operation lock
  // take this state mutex inside it; this function never acquires an outer lock.
  let lock;
  try {
    lock = acquireFileLock(stateLockPath(), { waitMs, purpose: 'config' });
    return saveConfigLocked(patch);
  } catch (error) {
    return { ok: false, config: null, error: `${error?.message ?? String(error)}; no configuration change was saved` };
  } finally {
    lock?.release();
  }
}

function saveConfigLocked(patch) {
  const file = configPath();
  const document = readJsonDocument(file);
  if (document.state !== 'ok' && document.state !== 'missing') {
    return { ok: false, config: null, error: `${document.error}; refusing to overwrite it with a partial document` };
  }
  const stored = document.state === 'ok' ? document.value : {};
  if (!isPlainObject(stored)) {
    return { ok: false, config: null, error: `${file} is not a JSON object; refusing to overwrite it` };
  }
  const merged = typeof patch === 'function' ? patch(stored) : { ...stored, ...patch };
  const normalized = normalizeConfig(merged, { present: true });
  if (!normalized.ok) {
    return {
      ok: false,
      config: normalized.config,
      errors: normalized.errors,
      error: 'the change would leave config.json in a state this agent cannot use, so nothing was written',
    };
  }
  const written = writeJsonAtomic(file, merged);
  if (!written.ok) return { ok: false, config: normalized.config, error: written.error };
  rememberLastGood(`${JSON.stringify(merged, null, 2)}\n`);
  return { ok: true, config: normalized.config, error: null };
}
