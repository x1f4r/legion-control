// What a reply may say about how this machine is configured.
//
// config.json holds the argv of every service, probe and action, and some of
// those arguments are private: a token on a command line, a password inside a
// URL, a path that names a person. The agent quotes its configuration back in
// free text all the time, because a person debugging a failed update needs to
// know which command failed, and a restricted key can read every one of those
// replies: status, op, history and logs are on its allowlist. Before 3.1.0
// only doctor and the bundle redacted anything (T7).
//
// So every reply to a restricted session goes through redactReply() before it
// is written (contract section 13), and doctor and the bundle use the same
// functions in every session. Redaction replaces each configured value that
// could be private wherever it appears in free text, and leaves alone every
// value a client decodes rather than reads, so a redacted reply still holds
// to its schema.
//
// It can only recognise what the configuration says. A secret that appears
// only in a command's own output is not something this file can know about,
// and the contract says so.

import { readJsonDocument } from './config-store.mjs';
import { configPath, lastGoodConfigPath } from './paths.mjs';

export const REDACTED = '<redacted>';

/** Whether a configuration key names a credential, whatever its spelling. */
export const sensitiveKey = (key) => /token|secret|password|passwd|apikey|authorization|credential|identityfile|privatekey|^auth$|^key$/i.test(key.replace(/[_-]/g, ''));

/**
 * Values shorter than this match only as a whole word: between characters
 * that are not letters or digits, or the ends of the text. A configured `-v`
 * or `on` would otherwise turn every word that contains it into nonsense.
 */
const SHORT_VALUE = 4;

const escape = (value) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/**
 * The keys config.mjs reads as one command, an argv, and `before`, which
 * holds a list of them. A command written there as one string is refused, and
 * the refusal shows how to write it by quoting it word by word; an array that
 * holds something other than strings runs with those items left out. Either
 * way, the words after the first are as private as an argv's arguments.
 */
const COMMAND_KEYS = new Set([
  'start', 'stop', 'running', 'command', 'installedVersion', 'latestVersion', 'update', 'verify', 'rollback', 'arm', 'reboot', 'rebootHelper',
]);
const COMMAND_LIST_KEYS = new Set(['before']);

/**
 * The user and password of a URL as it is written: everything between the
 * scheme's `//` and the last `@` before the path, query or fragment. A URL
 * the parser refuses (a port out of range, a space in the host) still shows
 * them to whoever reads it quoted back, so they are taken from the text too.
 */
const WRITTEN_USERINFO = /^\s*[A-Za-z][A-Za-z0-9+.-]*:\/\/([^/?#]*)@/;

/**
 * A function that redacts the configured values of `sources` from any string,
 * array or object it is given, without changing the original.
 *
 * Collected are every argument of a configured argv after its executable,
 * every word after the first of a command written as one string, the value of
 * every assignment among those arguments, every value under a credential-named
 * key, the user, password and credential-named query values inside every
 * URL, and the user and password of one that does not parse. This cannot
 * identify a secret that appears only in a command's output.
 */
export function valueRedactor(...sources) {
  const values = new Set();
  const remember = (value) => {
    if (typeof value === 'string' && value.length > 0) {
      values.add(value);
      // Both raw text and JSON-escaped echoes can occur in captured output.
      values.add(JSON.stringify(value).slice(1, -1));
    }
  };
  const argument = (item) => {
    collect(item, true);
    const assignment = typeof item === 'string' ? /^[^=\s]+=(.*)$/s.exec(item) : null;
    if (assignment) remember(assignment[1]);
  };
  const credential = (value) => {
    remember(value);
    try { remember(decodeURIComponent(value)); } catch { /* retain encoded value */ }
  };
  const collect = (value, sensitive = false, shape = null) => {
    if (typeof value === 'string') {
      if (sensitive) remember(value);
      if (shape) value.split(/\s+/).filter(Boolean).slice(1).forEach(argument);
      const written = WRITTEN_USERINFO.exec(value);
      if (written) {
        const [user, ...password] = written[1].split(':');
        [written[1], user, password.join(':')].forEach(credential);
      }
      try {
        const url = new URL(value);
        [url.username, url.password].forEach(credential);
        for (const [key, inner] of url.searchParams) if (sensitiveKey(key)) remember(inner);
        for (const pair of url.search.slice(1).split('&')) {
          const separator = pair.indexOf('=');
          if (separator < 0) continue;
          try {
            if (sensitiveKey(decodeURIComponent(pair.slice(0, separator).replace(/\+/g, ' ')))) remember(pair.slice(separator + 1));
          } catch { /* malformed query encoding is retained as unknown text */ }
        }
      } catch { /* most config strings are not URLs */ }
    } else if (Array.isArray(value)) {
      const argv = shape === 'command' || value.every((item) => typeof item === 'string');
      value.forEach((item, index) => {
        if (argv && index > 0) argument(item);
        else collect(item, sensitive);
        if (shape === 'commands') collect(item, sensitive, 'command');
      });
    } else if (value && typeof value === 'object') {
      for (const [key, inner] of Object.entries(value)) {
        const inside = COMMAND_KEYS.has(key) ? 'command' : COMMAND_LIST_KEYS.has(key) ? 'commands' : null;
        collect(inner, sensitive || sensitiveKey(key), inside);
      }
    }
  };
  sources.forEach((source) => collect(source));
  // Longest first, so a value is never left half replaced because a shorter
  // one inside it matched first.
  const alternatives = [...values].sort((a, b) => b.length - a.length).map((value) => (
    [...value].length < SHORT_VALUE ? `(?<![A-Za-z0-9])${escape(value)}(?![A-Za-z0-9])` : escape(value)
  ));
  const pattern = alternatives.length ? new RegExp(alternatives.join('|'), 'g') : null;
  const walk = (value) => {
    if (typeof value === 'string') return pattern ? value.split(REDACTED).map((part) => part.replace(pattern, () => REDACTED)).join(REDACTED) : value;
    if (Array.isArray(value)) return value.map(walk);
    if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, inner]) => [key, walk(inner)]));
    return value;
  };
  return walk;
}

/**
 * The redactor for a restricted session: the values of config.json as it is
 * now, and of the last copy that loaded cleanly. A config.json that no longer
 * parses still has to keep yesterday's secrets out of the log lines and
 * operation records that quote it.
 */
export function restrictedRedactor() {
  return valueRedactor(readJsonDocument(configPath()).value, readJsonDocument(lastGoodConfigPath()).value);
}

/**
 * Keys whose values a client decodes rather than reads: identifiers, enum
 * words, versions, timestamps, hashes, the names a reply is about, and the
 * setup document, which a restricted key reads with `config` anyway. A
 * configured argument that happens to be a word like "restart" or "busy" is
 * still redacted wherever it appears in prose, but rewriting it inside these
 * turned an operation's kind into "re<redacted>" and a reason code into
 * something no schema allows, and the reply into one no client decodes.
 */
const VERBATIM_KEYS = new Set([
  'agentVersion', 'system', 'reasonCode', 'deferredReason', 'action', 'outcome', 'state', 'phase', 'kind', 'mode', 'evidence', 'level', 'disposition',
  'id', 'opId', 'service', 'target', 'actionId', 'systemId', 'hash', 'keyFingerprint', 'lineage', 'source', 'version', 'from', 'to',
  'installed', 'latest', 'nightly', 'staged', 'channel', 'pendingVersion', 'replaced', 'floor', 'signed',
  'requestedAt', 'startedAt', 'updatedAt', 'finishedAt', 'expiresAt', 'checkedAt', 'at', 'nextWindow', 'pauseUntil', 'generatedAt', 'days',
  'controller', 'raw', 'meta', 'current', 'command', 'argument', 'accepts', 'flags', 'name', 'os', 'node', 'unit', 'capabilities', 'launchChanges', 'path',
]);

/** Redact the free text of a reply with `redact`, leaving every value under VERBATIM_KEYS as it is. */
export function redactReply(value, redact, key = null) {
  if (key !== null && VERBATIM_KEYS.has(key)) return value;
  if (Array.isArray(value)) return value.map((item) => redactReply(item, redact));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([name, inner]) => [name, redactReply(inner, redact, name)]));
  }
  return redact(value);
}

/** Keep executable names for diagnosis; every subsequent argument is private. */
export function redactConfig(config) {
  if (!config) return null;
  const walk = (value, key = '') => {
    if (sensitiveKey(key)) return REDACTED;
    if (Array.isArray(value)) {
      if (value.every((item) => typeof item === 'string')) return value.map((item, index) => index === 0 ? item : REDACTED);
      return value.map((item) => walk(item));
    }
    if (value && typeof value === 'object') {
      const out = {};
      for (const [name, inner] of Object.entries(value)) out[name] = walk(inner, name);
      return out;
    }
    return value;
  };

  return valueRedactor(config)(walk(config));
}
