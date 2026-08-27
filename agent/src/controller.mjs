// The controller config, as carried by the machines.
//
// Writing the controller config once on the Mac and once on every phone is one
// time too many, so every agent keeps a copy of it at <base>/controller.json and
// serves it to whoever asks. The document is opaque here: the agent never reads
// what is in it, it only stores and returns the bytes it was given, and the two
// apps do all the interpreting. The one thing checked is that the bytes are a
// JSON object shaped like a controller config at all, so a truncated paste or a
// stray shell redirect cannot replace a good document with rubbish.
//
// The hash is what makes the whole arrangement work. The Mac is the source of
// truth; every reply that carries a hash lets a device compare it against the
// sha256 of the document it holds and push or fetch only when they differ. So
// the hash must always describe the bytes that are actually on disk, which is
// why storing hashes exactly what it writes rather than what it was handed.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { basePath } from './config.mjs';

/** Room for far more machines than anyone has, and small enough to refuse a mistake. */
export const MAX_CONTROLLER_BYTES = 1024 * 1024;

export function controllerPath() {
  return path.join(basePath(), 'controller.json');
}

function sha256(text) {
  return crypto.createHash('sha256').update(text, 'utf8').digest('hex');
}

/** The exact bytes a document is stored as: trimmed, with one trailing newline. */
function storedForm(text) {
  return `${text.trim()}\n`;
}

/**
 * What is stored here, if anything.
 *
 * A file that does not parse still reports its hash. It is a real file with real
 * bytes, and the point of the hash is to tell the Mac that what is here is not
 * what it has; answering null would say "nothing stored" and invite the same
 * unreadable file to sit there forever.
 */
export function readController() {
  let raw;
  try {
    raw = fs.readFileSync(controllerPath(), 'utf8');
  } catch {
    return { document: null, raw: null, hash: null };
  }

  const hash = sha256(raw);
  try {
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return { document: null, raw, hash, error: 'controller.json is not a JSON object' };
    }
    return { document: parsed, raw, hash };
  } catch (error) {
    return { document: null, raw, hash, error: `controller.json does not parse: ${error.message}` };
  }
}

/** Why this text is not a controller config, or null when it is one. */
function reject(text) {
  if (Buffer.byteLength(text, 'utf8') > MAX_CONTROLLER_BYTES) {
    return `the document is larger than ${MAX_CONTROLLER_BYTES} bytes`;
  }

  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    return `the document does not parse: ${error.message}`;
  }

  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return 'the document is not a JSON object';
  if (typeof parsed.version !== 'number') return 'the document has no "version" number';
  if (!Array.isArray(parsed.machines)) return 'the document has no "machines" array';
  return null;
}

/**
 * Store a controller config. The bytes are written as they were given, not
 * re-serialized: the Mac hashes its own file to decide whether to push, and a
 * round trip through JSON.stringify would change the bytes and leave the two
 * sides disagreeing forever over a document they both consider identical.
 *
 * The write is a temp file plus a rename, so a process killed mid-write cannot
 * leave a truncated document behind for the apps to choke on.
 */
export function storeController(text) {
  const failed = (error) => ({ ok: false, hash: null, bytes: 0, error });

  if (typeof text !== 'string') return failed('no document given');
  const reason = reject(text);
  if (reason) return failed(reason);

  const body = storedForm(text);
  const file = controllerPath();
  const tmp = `${file}.tmp`;
  try {
    fs.mkdirSync(basePath(), { recursive: true });
    fs.writeFileSync(tmp, body, 'utf8');
    fs.renameSync(tmp, file);
  } catch (error) {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      /* nothing useful to do */
    }
    return failed(`controller.json could not be written: ${error.message}`);
  }

  return { ok: true, hash: sha256(body), bytes: Buffer.byteLength(body, 'utf8'), error: null };
}
