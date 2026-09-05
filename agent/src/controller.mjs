// The controller config, as carried by the machines.
//
// Writing the controller config once on each device is one time too many, so
// every agent keeps a copy at <base>/controller.json and serves it to whoever
// asks. The document is opaque here: the agent never reads what is in it, it
// only stores and returns the bytes it was given, and the clients do all the
// interpreting.
//
// TWO THINGS MAKE THIS WORK, and both were broken before.
//
// THE CANONICAL BYTES. The Mac hashed its raw file; the agent hashed the file
// after trimming and adding a newline. A document with no final newline, or with
// a blank line at the end, therefore had one hash on one side and another on the
// other, the push looked like it had failed, and the Mac retried it every ten
// minutes forever. There is now exactly one canonical byte form, defined in
// contract/tools/canonical.mjs and implemented identically here and in all three
// clients, and contract/hash-vectors.json is what proves the four agree.
//
// THE LINEAGE. A hash proves difference, not freshness, and a revision NUMBER
// proves neither once more than one device can edit. Peer A at revision 5 edits
// to 6 and publishes to machines X and Y; peer B, offline, edits its own 5 to 6
// and publishes to Z; A then edits to 7 and reaches Z, where 7 > 6 fast-forwards
// and B's work is gone with nobody told. Numbers cannot detect divergence.
// ANCESTRY can. Each document carries the hashes of its parents, and a push is
// accepted only when what this machine holds is one of those parents. Everything
// else is either "you are behind" or "you two have diverged, a person decides".
// The agent never merges and never picks a winner.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { basePath } from './config.mjs';
import { REASON } from './contract.mjs';

/** Room for far more machines than anyone has, and small enough to refuse a mistake. */
export const MAX_CONTROLLER_BYTES = 1024 * 1024;

/** How many ancestors a document carries. Deep enough for weeks of edits. */
export const MAX_LINEAGE = 32;

const HASH_PATTERN = /^[0-9a-f]{64}$/;
const SETUP_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$/;
const CONTROLLER_SOURCES = ['mac', 'desktop', 'phone', 'cli', 'legacy'];

export function controllerPath() {
  return path.join(basePath(), 'controller.json');
}

export function controllerMetaPath() {
  return path.join(basePath(), 'controller.meta.json');
}

/**
 * The commit record. Its existence is the point of no return: before it is
 * there, nothing has been applied; once it is, readers can serve its committed
 * snapshot and the next mutation finishes a swap this process did not finish.
 */
function commitPath() {
  return path.join(basePath(), 'controller.commit.json');
}

/**
 * The exact set of code points stripped from the two ends of the document:
 * tab, line feed, vertical tab, form feed, carriage return, space.
 *
 * Spelled out rather than delegated to the language's own trim, because those do
 * not agree across languages: JavaScript's String.trim also strips U+00A0 and
 * U+FEFF, Java's String.strip follows Character.isWhitespace, and .NET's Trim
 * uses yet another table. A document beginning with a non-breaking space has to
 * hash the same on all four sides, so the set is fixed and nowhere else.
 */
export const TRIM_CODE_POINTS = ['\u0009', '\u000a', '\u000b', '\u000c', '\u000d', '\u0020'];
const TRIM = new Set(TRIM_CODE_POINTS);

/** Whether these bytes are valid UTF-8. Invalid bytes are refused, never hashed. */
export function isValidUtf8(bytes) {
  const buffer = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
  // A round trip through the decoder loses nothing when the input was valid, and
  // replaces every bad byte with U+FFFD when it was not.
  return Buffer.compare(Buffer.from(buffer.toString('utf8'), 'utf8'), buffer) === 0;
}

/**
 * The canonical bytes of a controller document.
 *
 * Throws on input that is not valid UTF-8: hashing replacement characters would
 * hand back a stable hash for a document nobody can read, and the two sides
 * would then agree forever about rubbish.
 */
export function canonicalBytes(input) {
  const buffer = Buffer.isBuffer(input) ? input : Buffer.from(input, 'utf8');
  if (!isValidUtf8(buffer)) throw new Error('the document is not valid UTF-8');

  let text = buffer.toString('utf8');

  // 1. One leading byte order mark, and only a leading one. A BOM further in is
  //    a zero width no-break space inside the document and stays where it is.
  if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);

  // 2. Line endings. CRLF first, then any remaining lone CR, so a CRLF file does
  //    not turn into a blank line between every pair of lines.
  text = text.replace(/\r\n/g, '\n').replace(/\r/g, '\n');

  // 3. Both ends, using the fixed set above.
  let start = 0;
  let end = text.length;
  while (start < end && TRIM.has(text[start])) start += 1;
  while (end > start && TRIM.has(text[end - 1])) end -= 1;
  text = text.slice(start, end);

  // 4. Exactly one trailing newline, whether the editor left none or twelve.
  return Buffer.from(`${text}\n`, 'utf8');
}

/** Lowercase hex sha256 of the canonical bytes. */
export function canonicalHash(input) {
  return crypto.createHash('sha256').update(canonicalBytes(input)).digest('hex');
}

// ---------------------------------------------------------------------------
// Crash-consistent commit
// ---------------------------------------------------------------------------

function writeFileAtomic(file, contents) {
  const tmp = `${file}.tmp-${process.pid}-${crypto.randomBytes(4).toString('hex')}`;
  try {
    const fd = fs.openSync(tmp, 'wx');
    try {
      fs.writeFileSync(fd, contents);
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    fs.renameSync(tmp, file);
    syncControllerDirectory();
  } catch (error) {
    try { fs.rmSync(tmp, { force: true }); } catch { /* preserve the original write failure */ }
    throw error;
  }
}

function syncControllerDirectory() {
  try {
    const fd = fs.openSync(basePath(), 'r');
    try { fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
  } catch { /* directory fsync is unavailable on some platforms */ }
}

/** Metadata missing from a pre-lineage copy carries no ownership claim. */
function legacyMeta() {
  return { id: null, revision: 0, updatedAt: null, source: null, hash: null, lineage: [], device: null, bytes: 0 };
}

function readSnapshotFile(file) {
  try { return { state: 'present', bytes: fs.readFileSync(file) }; }
  catch (error) {
    return error.code === 'ENOENT'
      ? { state: 'missing', bytes: null }
      : { state: 'error', bytes: null, error: `${path.basename(file)} could not be read: ${error.message}` };
  }
}

function sameSnapshotFile(first, second) {
  return first.state === second.state && (first.state === 'present' ? first.bytes.equals(second.bytes) : first.error === second.error);
}

function metadataFrom(value, { required = false } = {}) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('controller metadata is not a JSON object');
  const meta = {
    id: typeof value.id === 'string' ? value.id : null,
    revision: Number.isInteger(value.revision) ? value.revision : 0,
    updatedAt: typeof value.updatedAt === 'string' ? value.updatedAt : null,
    source: typeof value.source === 'string' ? value.source : null,
    hash: typeof value.hash === 'string' ? value.hash : null,
    lineage: Array.isArray(value.lineage) ? value.lineage.filter((entry) => HASH_PATTERN.test(entry)) : [],
    device: typeof value.device === 'string' ? value.device : null,
    bytes: Number.isInteger(value.bytes) ? value.bytes : 0,
  };
  if ((required || meta.id !== null || meta.hash !== null) && !HASH_PATTERN.test(meta.hash ?? '')) {
    throw new Error('controller metadata has no valid document hash');
  }
  if (meta.id !== null && !SETUP_ID_PATTERN.test(meta.id)) throw new Error('controller metadata has an invalid setup id');
  if (meta.revision < 0 || (required && !Number.isInteger(value.revision))) throw new Error('controller metadata has an invalid revision');
  if (required && (!Number.isInteger(value.bytes) || value.bytes <= 0 || value.bytes > MAX_CONTROLLER_BYTES)) {
    throw new Error('controller metadata has an invalid canonical byte length');
  }
  if (value.lineage !== undefined && (!Array.isArray(value.lineage) || meta.lineage.length !== value.lineage.length)) {
    throw new Error('controller metadata has an invalid lineage');
  }
  return meta;
}

function unavailableSnapshot(error, raw = null, hash = null) {
  return { document: null, raw, hash, meta: legacyMeta(), consistent: false, error };
}

/** Parse one already-selected document, checking every metadata claim it binds. */
function documentSnapshot(bytes, meta, { requireMatch = false } = {}) {
  if (!bytes) {
    return requireMatch
      ? unavailableSnapshot('controller metadata names a document that is missing')
      : { document: null, raw: null, hash: null, meta, consistent: true };
  }
  let body;
  let hash;
  try {
    body = canonicalBytes(bytes);
    hash = crypto.createHash('sha256').update(body).digest('hex');
  } catch (error) {
    return { ...unavailableSnapshot(`controller.json ${error.message}`), consistent: !requireMatch };
  }
  const raw = bytes.toString('utf8');
  if (requireMatch && (hash !== meta.hash || (meta.bytes > 0 && meta.bytes !== body.length))) {
    return unavailableSnapshot('controller document and metadata do not describe the same canonical bytes', raw, hash);
  }
  let document;
  try {
    document = JSON.parse(raw);
    if (!document || typeof document !== 'object' || Array.isArray(document)) throw new Error('not a JSON object');
  } catch (error) {
    return { document: null, raw, hash, meta: requireMatch ? legacyMeta() : meta, consistent: !requireMatch,
      error: error.message === 'not a JSON object' ? 'controller.json is not a JSON object' : `controller.json does not parse: ${error.message}` };
  }
  if (requireMatch) {
    const block = readControllerBlock(document);
    if (block.problems.length > 0 || (block.id !== null && block.id !== meta.id) ||
        (block.revision !== null && block.revision !== meta.revision) ||
        (block.present && JSON.stringify(block.lineage) !== JSON.stringify(meta.lineage))) {
      return unavailableSnapshot('controller document and metadata disagree about setup identity, revision, or lineage', raw, hash);
    }
  }
  return { document, raw, hash, meta, consistent: true };
}

/** Select the journal's committed version without changing either live file. */
function pendingSnapshot(journal) {
  let commit;
  let meta;
  try {
    commit = JSON.parse(journal.bytes.toString('utf8'));
    meta = metadataFrom(commit?.meta, { required: true });
  } catch (error) {
    return { snapshot: unavailableSnapshot(`controller commit journal is unusable: ${error.message}`) };
  }
  let error = null;
  for (const file of [`${controllerPath()}.staged`, controllerPath()]) {
    const candidate = readSnapshotFile(file);
    if (candidate.state !== 'present') {
      error = candidate.error ?? error;
      continue;
    }
    const snapshot = documentSnapshot(candidate.bytes, meta, { requireMatch: true });
    if (snapshot.consistent && snapshot.document) return { snapshot, file, commit };
    error = snapshot.error;
  }
  return { snapshot: unavailableSnapshot(`controller commit has no matching staged or live document: ${error ?? 'both files are missing'}`) };
}

/**
 * Finish an interrupted commit. Only a caller holding the operation mutex may
 * invoke this; readers select the committed snapshot without renaming files.
 */
function replayCommit() {
  const journal = readSnapshotFile(commitPath());
  if (journal.state === 'missing') return;
  if (journal.state === 'error') throw new Error(journal.error);
  const selected = pendingSnapshot(journal);
  if (!selected.snapshot.consistent) throw new Error(selected.snapshot.error);
  if (!sameSnapshotFile(journal, readSnapshotFile(commitPath()))) throw new Error('controller commit changed during recovery');
  if (selected.file !== controllerPath()) fs.renameSync(selected.file, controllerPath());
  syncControllerDirectory();
  writeFileAtomic(controllerMetaPath(), `${JSON.stringify(selected.commit.meta, null, 2)}\n`);
  fs.rmSync(commitPath(), { force: true });
  syncControllerDirectory();
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/**
 * A crash-consistent read without acquiring a mutation lock or replaying writes.
 * A journal is the commit point: until its two renames finish, its hash selects
 * the matching staged or live document. Rechecking the journal and metadata
 * detects concurrent publication, and bounded retries either find a coherent
 * pair or report that the snapshot could not be established.
 */
export function readController() {
  let last = unavailableSnapshot('controller snapshot changed during the read; retry shortly');
  for (let attempt = 0; attempt < 4; attempt += 1) {
    const journal = readSnapshotFile(commitPath());
    if (journal.state === 'error') return unavailableSnapshot(journal.error);
    if (journal.state === 'present') {
      const selected = pendingSnapshot(journal).snapshot;
      if (!sameSnapshotFile(journal, readSnapshotFile(commitPath()))) continue;
      return selected;
    }

    const raw = readSnapshotFile(controllerPath());
    const metadata = readSnapshotFile(controllerMetaPath());
    let meta = legacyMeta();
    let failure = raw.state === 'error' ? raw.error : metadata.state === 'error' ? metadata.error : null;
    if (metadata.state === 'present') {
      try { meta = metadataFrom(JSON.parse(metadata.bytes.toString('utf8'))); }
      catch (error) { failure = `controller metadata is unusable: ${error.message}`; }
    }
    const snapshot = documentSnapshot(raw.bytes, meta, { requireMatch: meta.hash !== null });
    if (!sameSnapshotFile(metadata, readSnapshotFile(controllerMetaPath())) ||
        !sameSnapshotFile(journal, readSnapshotFile(commitPath()))) continue;
    if (failure) return unavailableSnapshot(failure, snapshot.raw, snapshot.hash);
    if (snapshot.consistent) return snapshot;
    last = snapshot;
  }
  return last;
}

/** Metadata is served only with a verified snapshot of the document it names. */
export function readControllerMeta() {
  const snapshot = readController();
  if (!snapshot.consistent) throw new Error(snapshot.error);
  return snapshot.meta;
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/** Why this text is not a controller config, or null when it is one. */
function reject(bytes) {
  if (bytes.length > MAX_CONTROLLER_BYTES) {
    return `the document is larger than ${MAX_CONTROLLER_BYTES} bytes`;
  }
  if (!isValidUtf8(bytes)) return 'the document is not valid UTF-8';

  let parsed;
  try {
    parsed = JSON.parse(bytes.toString('utf8'));
  } catch (error) {
    return `the document does not parse: ${error.message}`;
  }

  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return 'the document is not a JSON object';
  if (typeof parsed.version !== 'number') return 'the document has no "version" number';
  if (!Array.isArray(parsed.machines)) return 'the document has no "machines" array';
  return null;
}

/**
 * The `controller` block inside the document, validated.
 *
 * This is the one part of the document the agent reads. It still never looks at
 * `machines`: what it needs is the identity and the ancestry, because those are
 * what decide whether accepting this document would throw away somebody's work.
 */
export function readControllerBlock(parsed) {
  const problems = [];
  const block = parsed?.controller;
  if (block === undefined || block === null) return { present: false, id: null, revision: null, lineage: [], device: null, source: null, problems };
  if (typeof block !== 'object' || Array.isArray(block)) {
    problems.push({ path: 'controller', message: 'expected an object' });
    return { present: true, id: null, revision: null, lineage: [], device: null, source: null, problems };
  }

  const id = typeof block.id === 'string' ? block.id : null;
  if (id !== null && !SETUP_ID_PATTERN.test(id)) {
    problems.push({ path: 'controller.id', message: `${JSON.stringify(id)} is not a usable setup id` });
  }

  const revision = Number.isInteger(block.revision) ? block.revision : null;
  if (block.revision !== undefined && revision === null) {
    problems.push({ path: 'controller.revision', message: `expected a whole number, got ${JSON.stringify(block.revision)}` });
  }
  if (revision !== null && revision < 0) {
    problems.push({ path: 'controller.revision', message: 'a revision cannot be negative' });
  }

  let lineage = [];
  if (block.lineage !== undefined && block.lineage !== null) {
    if (!Array.isArray(block.lineage)) {
      problems.push({ path: 'controller.lineage', message: 'expected an array of sha256 hashes' });
    } else if (block.lineage.length > MAX_LINEAGE) {
      problems.push({ path: 'controller.lineage', message: `at most ${MAX_LINEAGE} ancestors, got ${block.lineage.length}` });
    } else {
      const bad = block.lineage.find((entry) => typeof entry !== 'string' || !HASH_PATTERN.test(entry));
      if (bad !== undefined) {
        problems.push({ path: 'controller.lineage', message: `${JSON.stringify(bad)} is not a lowercase hex sha256` });
      } else if (new Set(block.lineage).size !== block.lineage.length) {
        problems.push({ path: 'controller.lineage', message: 'the same ancestor appears twice' });
      } else {
        lineage = [...block.lineage];
      }
    }
  }

  const source = typeof block.source === 'string' ? block.source : null;
  if (source !== null && !CONTROLLER_SOURCES.includes(source)) {
    problems.push({ path: 'controller.source', message: `expected one of ${CONTROLLER_SOURCES.join(', ')}, got ${JSON.stringify(source)}` });
  }

  return {
    present: true,
    id,
    revision,
    lineage,
    device: typeof block.device === 'string' ? block.device : null,
    source,
    problems,
  };
}

/**
 * Decide whether an incoming document may replace the stored one.
 *
 * Pure, so every rule is testable without a filesystem. `held` is what this
 * machine has; `incoming` is what is being pushed. Returns `{ ok, action }` or
 * `{ ok: false, reasonCode, message, divergent }`.
 *
 * The rules, in order, and the order matters:
 *
 *   1. nothing held, or held with no identity   store. Any valid document beats
 *                                               nothing, and a 2.x copy has no
 *                                               ancestry to protect.
 *   2. a different setup id                     conflict, unless --replace. Two
 *                                               setups are not two versions of
 *                                               one, and picking either silently
 *                                               would be a guess about intent.
 *   3. the same bytes we already hold           noop. A retried push after a cut
 *                                               link must not look like an error.
 *   4. we hold one of its ancestors             store. This is a fast-forward:
 *                                               the pusher has our work and more.
 *   5. it is one of OUR ancestors               stale-revision. The pusher is
 *                                               behind; it should fetch.
 *   6. neither descends from the other          conflict, divergent. Both sides
 *                                               have work the other has not seen.
 *
 * Revision numbers never decide acceptance. They are checked only for sanity on
 * a fast-forward, where a descendant must carry a higher number than its parent.
 */
export function judgeIncoming(held, incoming) {
  const replace = incoming.replace === true;
  const heldLineage = Array.isArray(held.lineage) ? held.lineage : [];
  const pushedLineage = Array.isArray(incoming.lineage) ? incoming.lineage : [];

  // 1. Nothing to protect.
  if (!held.hash || held.id === null) return { ok: true, action: 'stored' };

  // 2. A different setup entirely.
  if (incoming.id !== null && incoming.id !== held.id) {
    if (!replace) {
      return {
        ok: false,
        reasonCode: REASON.controllerConflict,
        divergent: false,
        message:
          `this machine holds setup "${held.id}" and the document offered is "${incoming.id}". ` +
          'These are different setups, not two versions of one. Choose which one this machine should follow; --replace switches it',
      };
    }
    return { ok: true, action: 'replaced' };
  }

  // A push with no identity at all (a 2.x client) onto a copy that has one would
  // silently drop the ancestry every other device is comparing against.
  if (incoming.id === null) {
    if (!replace) {
      return {
        ok: false,
        reasonCode: REASON.controllerConflict,
        divergent: false,
        message:
          `this machine holds setup "${held.id}" at revision ${held.revision}, and the document offered carries no identity. ` +
          'Push from a client that sends --controller-id and --revision, or pass --replace to deliberately take over',
      };
    }
    return { ok: true, action: 'replaced' };
  }

  // 3. Byte-identical: the push already landed.
  if (incoming.hash === held.hash) return { ok: true, action: 'noop' };

  // 4. Fast-forward: what we hold is one of its parents.
  if (pushedLineage.includes(held.hash)) {
    if (incoming.revision !== null && incoming.revision <= held.revision) {
      return {
        ok: false,
        reasonCode: REASON.badArgument,
        divergent: false,
        message:
          `the document descends from what this machine holds but its revision (${incoming.revision}) is not above ${held.revision}. ` +
          'A descendant carries a higher revision than its parent',
      };
    }
    return { ok: true, action: 'stored' };
  }

  // 5. The pusher is behind: we already hold a descendant of what it offers.
  if (heldLineage.includes(incoming.hash)) {
    return {
      ok: false,
      reasonCode: REASON.staleRevision,
      divergent: false,
      message:
        `this machine already holds revision ${held.revision} of "${held.id}", which descends from the document offered. ` +
        'Fetch this machine\'s copy first',
    };
  }

  // 6. Real divergence. The agent never merges: only a person knows which of two
  // concurrent edits mattered, or that both did.
  if (!replace) {
    return {
      ok: false,
      reasonCode: REASON.controllerConflict,
      divergent: true,
      message:
        `this machine holds revision ${held.revision} of "${held.id}" and the document offered is revision ${incoming.revision ?? 'unnumbered'}, ` +
        'and neither descends from the other. Somebody edited the setup in two places without one seeing the other; merge them and publish the result',
    };
  }
  return { ok: true, action: 'replaced' };
}

/**
 * Store a controller config.
 *
 * The bytes written are the CANONICAL bytes, not the bytes as given and not a
 * round trip through JSON.stringify. Canonical because that is the one form
 * every side hashes; not re-serialized because reformatting somebody's document
 * would change it into a document they did not write.
 */
export function storeController(input, { id = null, revision = null, source = null, replace = false } = {}) {
  const failed = (error, reasonCode = REASON.badArgument, extra = {}) => ({
    ok: false,
    hash: null,
    bytes: 0,
    reasonCode,
    error,
    ...extra,
  });

  if (input === null || input === undefined) return failed('no document given');
  const raw = Buffer.isBuffer(input) ? input : Buffer.from(String(input), 'utf8');

  if (raw.length > MAX_CONTROLLER_BYTES) return failed(`the document is larger than ${MAX_CONTROLLER_BYTES} bytes`);
  if (!isValidUtf8(raw)) return failed('the document is not valid UTF-8');

  let parsed;
  try {
    parsed = JSON.parse(raw.toString('utf8'));
  } catch (error) {
    return failed(`the document does not parse: ${error.message}`);
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return failed('the document is not a JSON object');
  if (typeof parsed.version !== 'number') return failed('the document has no "version" number');
  if (!Array.isArray(parsed.machines)) return failed('the document has no "machines" array');

  const block = readControllerBlock(parsed);
  if (block.problems.length > 0) {
    return failed(
      `the document's controller block is not usable: ${block.problems.map((p) => `${p.path}: ${p.message}`).join('; ')}`,
      REASON.configInvalid,
    );
  }

  // --controller-id and --revision go together, and they have to agree with the
  // block inside the document. Two places saying different things about the same
  // revision is exactly the ambiguity the lineage rules exist to remove.
  if ((id === null) !== (revision === null)) {
    return failed('--controller-id and --revision go together; give both or neither');
  }
  if (id !== null && !SETUP_ID_PATTERN.test(id)) {
    return failed(`the controller id ${JSON.stringify(id)} is not a usable identifier`);
  }
  if (revision !== null && (!Number.isInteger(revision) || revision < 0)) {
    return failed(`the revision must be a whole number of zero or more, got ${JSON.stringify(revision)}`);
  }
  if (id !== null) {
    if (block.id !== null && block.id !== id) {
      return failed(`--controller-id is "${id}" but the document says "${block.id}"`);
    }
    if (block.revision !== null && block.revision !== revision) {
      return failed(`--revision is ${revision} but the document says ${block.revision}`);
    }
  }

  const effectiveId = id ?? block.id;
  const effectiveRevision = revision ?? block.revision;
  const lineage = block.lineage;

  const body = canonicalBytes(raw);
  const hash = crypto.createHash('sha256').update(body).digest('hex');
  // Publication already owns the operation mutex. Complete an earlier committed
  // transaction before judging ancestry, but never do that from a read path.
  try { replayCommit(); }
  catch (error) { return failed(`the previous controller commit could not be completed: ${error.message}`, REASON.internal); }
  const current = readController();
  if (!current.consistent) return failed(current.error, REASON.internal);
  const held = current.meta;
  const verdict = judgeIncoming(held, {
    id: effectiveId,
    revision: effectiveRevision,
    hash,
    lineage,
    replace,
  });

  if (!verdict.ok) {
    return {
      ok: false,
      hash: null,
      bytes: 0,
      reasonCode: verdict.reasonCode,
      divergent: verdict.divergent === true,
      error: verdict.message,
      current: held,
    };
  }

  // Idempotent re-push: the bytes are already here, so there is nothing to write
  // and nothing to change. A client whose link was cut mid-push retries and gets
  // this rather than an error it would have to interpret.
  if (verdict.action === 'noop') {
    return { ok: true, action: 'noop', hash, bytes: body.length, meta: held, error: null };
  }

  const meta = {
    id: effectiveId,
    revision: effectiveRevision ?? 0,
    updatedAt: new Date().toISOString(),
    source: source ?? block.source ?? (effectiveId === null ? 'legacy' : 'cli'),
    hash,
    lineage,
    device: block.device,
    bytes: body.length,
  };

  try {
    fs.mkdirSync(basePath(), { recursive: true });
    // 1. Stage the document beside its destination.
    const staged = `${controllerPath()}.staged`;
    writeFileAtomic(staged, body);
    // 2. The commit record. Everything after this point is replayable, so a
    //    reader can never see a document whose metadata describes another one.
    writeFileAtomic(commitPath(), `${JSON.stringify({ meta }, null, 2)}\n`);
    // 3. Apply, and clear the record.
    replayCommit();
  } catch (error) {
    // A published journal is a committed version even if applying its renames
    // fails. Keep it and its staged document for read-only snapshots and the
    // next mutation to recover; never erase evidence of an accepted commit.
    return failed(`controller.json could not be written: ${error.message}`, REASON.internal);
  }

  return { ok: true, hash, bytes: body.length, action: verdict.action, meta, error: null };
}
