// Launch values and local approval: the reference of v3.md section 9.9.
//
// The shared setup carries the argv each device runs over ssh with its own
// key, and lineage only proves that a document claims an ancestor, not who
// wrote it. Before this, any key allowed to run `config set`, a restricted one
// included, could publish a descendant whose `agent` was `sh -c …`, and every
// peer adopted it and ran it on every machine with an unrestricted key (T1).
// The document still replicates unchanged; what changes is that a device runs
// only the launch values it approved itself, and everything else waits for a
// person on that device.
//
// This file is the function every implementation agrees on: which values are
// launch values and how they are read from a document (the projection), which
// offered values are accepted without asking, which wait for review and
// whether they could be approved at all, and what a device runs meanwhile (the
// effective values). It is written apart from agent/src/launch.mjs, which
// implements the same rules for the agent's restricted check, so the two can be
// held to each other through contract/vectors/launch.json.
//
// No dependencies, no clock, no environment. Values are compared as JSON:
// arrays in order, strings by code point, with no case folding and no path or
// Unicode normalisation, because two spellings a person would call the same
// path are two different files to a file system that does not normalise.

import fs from 'node:fs';
import { canonicalHash } from './canonical.mjs';
import { GRAMMARS, resolveGrammar } from './transport.mjs';

export const FAMILIES = ['launch'];

/** The repository a client updates from when the setup names none. */
export const DEFAULT_REPO = 'x1f4r/legion-control';

// The projection reads the grammar the same way section 15.1 does; the agent's
// own launch.mjs carries the same function.
export { resolveGrammar };

const COMMON = JSON.parse(fs.readFileSync(new URL('../schemas/common.schema.json', import.meta.url), 'utf8'));
const REPO = new RegExp(COMMON.$defs.githubRepo.pattern, 'u');

const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const own = (object, key) => (isPlainObject(object) && Object.hasOwn(object, key) ? object[key] : undefined);
// A record is keyed by ids taken from the document. defineProperty keeps an id
// such as "__proto__" an ordinary key instead of a prototype assignment.
const put = (object, key, value) => Object.defineProperty(object, key, { value, enumerable: true, writable: true, configurable: true });

// ---------------------------------------------------------------------------
// Paths. Ids are written as they are, inside the brackets, never joined or
// escaped: an id may contain a slash or a dot, and the record nests them as
// keys for that reason (common.schema.json#/$defs/launchPath).
// ---------------------------------------------------------------------------

export const systemPath = (machine, system) => `machines[${machine}].systems[${system}]`;
export const identityFilePath = (machine) => `machines[${machine}].ssh.identityFile`;
export const REPO_PATH = 'appUpdates.githubRepo';
export const LOCAL_PATH = 'local';

// ---------------------------------------------------------------------------
// What can be approved at all.
// ---------------------------------------------------------------------------

// Control characters, C1 controls, zero-width and bidirectional formatting
// characters, word joiners and the byte order mark. Each can make the text a
// person approves look different from the text that runs; a value holding one
// can only be reverted or left, never approved.
const LAUNCH_TEXT = /^[^\u0000-\u001F\u007F-\u009F\u200B-\u200F\u202A-\u202E\u2060-\u2064\u2066-\u2069\uFEFF]+$/u;

/** Whether an argv item, an identityFile or local.agent can be shown to a person and approved. */
export function isLaunchText(text) {
  if (typeof text !== 'string' || /[\n\r]/.test(text)) return false;
  const size = Buffer.byteLength(text, 'utf8');
  return size >= 1 && size <= 4096 && LAUNCH_TEXT.test(text);
}

const isRepo = (value) => typeof value === 'string' && !/[\n\r]/.test(value) && REPO.test(value);

/** A tuple a record or a projection can hold: a non-empty argv of strings and a grammar. */
const isTuple = (value) => isPlainObject(value) && Array.isArray(value.agent) && value.agent.length > 0
  && value.agent.every((item) => typeof item === 'string') && typeof value.grammar === 'string';

const sameTuple = (a, b) => a.grammar === b.grammar && a.agent.length === b.agent.length
  && a.agent.every((item, index) => item === b.agent[index]);

/** A tuple that can run: every argv item shown faithfully, and a grammar section 15.2 can write. */
const runnableTuple = (tuple) => isTuple(tuple) && tuple.agent.every(isLaunchText) && GRAMMARS.includes(tuple.grammar);

// ---------------------------------------------------------------------------
// The projection.
// ---------------------------------------------------------------------------

function tupleOf(system) {
  const { agent } = system;
  // Not an array, empty, or holding anything but strings: nothing runs on this
  // system, so the tuple is inert and accepted wherever it appears.
  if (!Array.isArray(agent) || agent.length === 0 || !agent.every((item) => typeof item === 'string')) {
    return { agent: null, grammar: null };
  }
  // The grammar, not the raw platform, shell and restricted fields, is what
  // decides how the line is quoted. Recording the grammar means an editor that
  // writes `shell: "cmd"` where it was absent on Windows changes nothing that
  // runs and asks nobody anything.
  const r = resolveGrammar({
    platform: system.platform, shell: system.shell, restricted: system.restricted,
    restrictedSession: false, agentLength: agent.length,
  });
  return { agent: [...agent], grammar: r.grammar ?? `refused:${r.refusal}` };
}

/**
 * The launch values of a parsed document (section 9.9, "Reading them"):
 * `{ machines: [{ id, systems: [{ id, tuple }], identityFile }], repo, local }`,
 * where `identityFile` is null when absent, `repo` null means the built-in
 * repository and `local` null means none.
 *
 * Always the parsed JSON, never a typed model: a model fills in defaults and a
 * lenient decoder drops what it cannot read, and either would make the
 * projection disagree with what the next device reads from the same bytes.
 * Machines and systems are walked in order; a non-object, an item without a
 * string id and a repeated id (after its first occurrence) are skipped.
 */
export function launchProjection(document) {
  const machines = [];
  const seenMachines = new Set();
  for (const machine of Array.isArray(document?.machines) ? document.machines : []) {
    if (!isPlainObject(machine) || typeof machine.id !== 'string' || seenMachines.has(machine.id)) continue;
    seenMachines.add(machine.id);
    const systems = [];
    const seenSystems = new Set();
    for (const system of Array.isArray(machine.systems) ? machine.systems : []) {
      if (!isPlainObject(system) || typeof system.id !== 'string' || seenSystems.has(system.id)) continue;
      seenSystems.add(system.id);
      systems.push({ id: system.id, tuple: tupleOf(system) });
    }
    const identityFile = own(machine.ssh, 'identityFile');
    machines.push({ id: machine.id, systems, identityFile: typeof identityFile === 'string' && identityFile.length > 0 ? identityFile : null });
  }
  const githubRepo = own(document?.appUpdates, 'githubRepo');
  const repo = typeof githubRepo === 'string' && githubRepo.length > 0 && githubRepo !== DEFAULT_REPO ? githubRepo : null;
  const local = isPlainObject(document?.local) && document.local.enabled !== false
    ? { agent: typeof document.local.agent === 'string' && document.local.agent.length > 0 ? document.local.agent : null }
    : null;
  return { machines, repo, local };
}

// ---------------------------------------------------------------------------
// The approval record.
// ---------------------------------------------------------------------------

/**
 * A record (v3.md section 11) approving every launch value of `document`: what
 * a device writes when it seeds itself from the document it already applies,
 * and what the agent compares a restricted push with. An inert tuple runs
 * nothing and is accepted anyway, so it is not recorded. `seededFrom` is the
 * hash of the seeded document, or "none".
 */
export function approvalsFrom(document, { seededFrom = 'none' } = {}) {
  const projection = launchProjection(document);
  const record = { schema: 1, seededFrom };
  const machines = {};
  for (const machine of projection.machines) {
    const entry = {};
    const systems = {};
    for (const system of machine.systems) {
      if (system.tuple.agent !== null) put(systems, system.id, { agent: [...system.tuple.agent], grammar: system.tuple.grammar });
    }
    if (Object.keys(systems).length > 0) entry.systems = systems;
    if (machine.identityFile !== null) entry.identityFile = machine.identityFile;
    if (Object.keys(entry).length > 0) put(machines, machine.id, entry);
  }
  if (Object.keys(machines).length > 0) record.machines = machines;
  if (projection.repo !== null) record.githubRepo = projection.repo;
  if (projection.local !== null) record.local = { agent: projection.local.agent };
  return record;
}

/** Every approved value of a record, wherever it sits: a value approved anywhere is accepted anywhere. */
function approvedValues(store) {
  const tuples = [];
  const identityFiles = [];
  const machines = own(store, 'machines');
  if (isPlainObject(machines)) {
    for (const machine of Object.values(machines)) {
      const systems = own(machine, 'systems');
      if (isPlainObject(systems)) for (const tuple of Object.values(systems)) if (isTuple(tuple)) tuples.push(tuple);
      const identityFile = own(machine, 'identityFile');
      if (typeof identityFile === 'string') identityFiles.push(identityFile);
    }
  }
  const repo = own(store, 'githubRepo');
  const local = own(store, 'local');
  return {
    tuples,
    identityFiles,
    repo: typeof repo === 'string' ? repo : null,
    local: isPlainObject(local) ? local : null,
  };
}

/** What the record holds at one location: the fallback for a value that waits there. */
const recordedTuple = (store, machine, system) => own(own(own(own(store, 'machines'), machine), 'systems'), system);
const recordedIdentityFile = (store, machine) => own(own(own(store, 'machines'), machine), 'identityFile');

// ---------------------------------------------------------------------------
// Acceptance.
// ---------------------------------------------------------------------------

/**
 * Whether `offered` is the launcher migration of `approved`: the same grammar,
 * the same argv except one item, which moves from `<base>/agent/src/index.mjs`
 * to `<base>/bin/launcher.mjs` under the same non-empty base with no `..`
 * segment, compared after replacing `\` with `/` because the desktop writes
 * the new path with forward slashes into a Windows argv. The 3.1 upgrade
 * migrates every system this way, and asking every peer to review a file the
 * agent itself installs under an already approved base would add prompts and
 * no protection. The reverse direction is never a migration.
 */
export function isLauncherMigration(approved, offered) {
  if (!isTuple(approved) || !isTuple(offered)) return false;
  if (approved.grammar !== offered.grammar || approved.agent.length !== offered.agent.length) return false;
  const differing = approved.agent.flatMap((item, index) => (item === offered.agent[index] ? [] : [index]));
  if (differing.length !== 1) return false;
  const from = approved.agent[differing[0]].replaceAll('\\', '/');
  const to = offered.agent[differing[0]].replaceAll('\\', '/');
  const ENTRY = '/agent/src/index.mjs';
  const LAUNCHER = '/bin/launcher.mjs';
  if (!from.endsWith(ENTRY) || !to.endsWith(LAUNCHER)) return false;
  const base = from.slice(0, -ENTRY.length);
  return base.length > 0 && base === to.slice(0, -LAUNCHER.length) && !base.split('/').includes('..');
}

const acceptedTuple = (approved, tuple) => tuple.agent === null
  || approved.tuples.some((candidate) => sameTuple(candidate, tuple) || isLauncherMigration(candidate, tuple));
const acceptedIdentityFile = (approved, path) => path === null || approved.identityFiles.includes(path);
const acceptedRepo = (approved, repo) => repo === null || approved.repo === repo;
const acceptedLocal = (approved, local) => local === null || local.agent === null || (approved.local !== null && approved.local.agent === local.agent);

const approvableTuple = (tuple) => tuple.agent === null || runnableTuple(tuple);
const approvableLocal = (local) => local.agent === null || isLaunchText(local.agent);

/**
 * One entry for each offered launch value that is not accepted without asking,
 * in this order: for each machine its systems and then its identityFile; then
 * the repository; then local.
 *
 * `{ path, scope, kind, approvable, approved?, offered }`: `kind` is `changed`
 * when the record holds a value at that path and `new` otherwise; `approved` is
 * that value. `approvable` is false for a value that can only be reverted or
 * left: an argv item, identityFile or local.agent that fails isLaunchText, a
 * grammar section 15.1 refuses, a repository that is not owner/name.
 */
export function launchReview(store, projection) {
  const approved = approvedValues(store);
  const entries = [];
  const entry = (path, scope, recorded, approvable, offered) => {
    entries.push({
      path, scope,
      kind: recorded === undefined ? 'new' : 'changed',
      approvable,
      ...(recorded === undefined ? {} : { approved: recorded }),
      offered,
    });
  };
  for (const machine of projection.machines) {
    for (const system of machine.systems) {
      if (acceptedTuple(approved, system.tuple)) continue;
      entry(systemPath(machine.id, system.id), 'system', recordedTuple(store, machine.id, system.id), approvableTuple(system.tuple), system.tuple);
    }
    if (!acceptedIdentityFile(approved, machine.identityFile)) {
      entry(identityFilePath(machine.id), 'identityFile', recordedIdentityFile(store, machine.id), isLaunchText(machine.identityFile), machine.identityFile);
    }
  }
  if (!acceptedRepo(approved, projection.repo)) {
    entry(REPO_PATH, 'repo', own(store, 'githubRepo'), isRepo(projection.repo), projection.repo);
  }
  if (!acceptedLocal(approved, projection.local)) {
    entry(LOCAL_PATH, 'local', own(store, 'local'), approvableLocal(projection.local), projection.local);
  }
  return entries;
}

/**
 * What this device runs (section 9.9, "Everything else waits"): one entry per
 * offered system, then one identityFile entry per machine, then the repository,
 * then local, each `{ path, scope, state, agent, grammar, text }`.
 *
 * An accepted value that can run is used as offered (`offered`). Anything
 * else falls back to what the record holds at that same location
 * (`approved`), never at another one, and only when the recorded value can run
 * itself; with nothing usable there a system is `not-driven`, an identityFile
 * or local is `none` and the repository is `built-in`. An inert tuple is
 * `not-driven`: nothing runs on it. `agent` and `grammar` are set only for a
 * system that runs; `text` holds the identityFile path, the repository, or
 * local.agent, null for local's default path and for the built-in repository.
 */
export function effectiveLaunch(store, projection) {
  const approved = approvedValues(store);
  const entries = [];
  for (const machine of projection.machines) {
    for (const system of machine.systems) {
      const path = systemPath(machine.id, system.id);
      const { tuple } = system;
      let state = 'not-driven';
      let used = null;
      if (tuple.agent !== null) {
        if (acceptedTuple(approved, tuple) && runnableTuple(tuple)) {
          state = 'offered';
          used = tuple;
        } else {
          const recorded = recordedTuple(store, machine.id, system.id);
          if (runnableTuple(recorded)) {
            state = 'approved';
            used = recorded;
          }
        }
      }
      entries.push({ path, scope: 'system', state, agent: used === null ? null : [...used.agent], grammar: used === null ? null : used.grammar, text: null });
    }
  }
  for (const machine of projection.machines) {
    const path = identityFilePath(machine.id);
    const offered = machine.identityFile;
    const recorded = recordedIdentityFile(store, machine.id);
    if (offered !== null && acceptedIdentityFile(approved, offered) && isLaunchText(offered)) {
      entries.push({ path, scope: 'identityFile', state: 'offered', agent: null, grammar: null, text: offered });
    } else if (offered !== null && isLaunchText(recorded)) {
      entries.push({ path, scope: 'identityFile', state: 'approved', agent: null, grammar: null, text: recorded });
    } else {
      entries.push({ path, scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null });
    }
  }
  {
    const offered = projection.repo;
    const recorded = own(store, 'githubRepo');
    if (offered !== null && acceptedRepo(approved, offered) && isRepo(offered)) {
      entries.push({ path: REPO_PATH, scope: 'repo', state: 'offered', agent: null, grammar: null, text: offered });
    } else if (offered !== null && isRepo(recorded) && recorded !== DEFAULT_REPO) {
      entries.push({ path: REPO_PATH, scope: 'repo', state: 'approved', agent: null, grammar: null, text: recorded });
    } else {
      entries.push({ path: REPO_PATH, scope: 'repo', state: 'built-in', agent: null, grammar: null, text: null });
    }
  }
  {
    const offered = projection.local;
    const recorded = own(store, 'local');
    const recordedRunnable = isPlainObject(recorded) && (recorded.agent === null || isLaunchText(recorded.agent));
    if (offered !== null && acceptedLocal(approved, offered) && approvableLocal(offered)) {
      entries.push({ path: LOCAL_PATH, scope: 'local', state: 'offered', agent: null, grammar: null, text: offered.agent });
    } else if (offered !== null && recordedRunnable) {
      entries.push({ path: LOCAL_PATH, scope: 'local', state: 'approved', agent: null, grammar: null, text: recorded.agent });
    } else {
      entries.push({ path: LOCAL_PATH, scope: 'local', state: 'none', agent: null, grammar: null, text: null });
    }
  }
  return entries;
}

// ---------------------------------------------------------------------------
// The reference interface of the `launch` vector family.
// ---------------------------------------------------------------------------

/**
 * `review`: the record is `approvals` when given, otherwise the record seeded
 * from the held document, otherwise an empty one. `restrictedRefused` is what
 * agent 3.1.0 answers a restricted session that pushes the offered document
 * over the held one (section 9.4): refused exactly when a launch value would
 * wait for review against the held document's own values. It never depends on
 * `approvals`, which is one device's record and not the machine's.
 */
export function run(group, input) {
  if (group === 'migration') return { migration: isLauncherMigration(input.approved, input.offered) };
  if (group !== 'review') throw new Error(`the launch family has no group "${group}"`);
  const held = input.heldText === null ? null : JSON.parse(input.heldText);
  const store = input.approvals
    ?? (held === null ? { schema: 1, seededFrom: 'none' } : approvalsFrom(held, { seededFrom: canonicalHash(input.heldText) }));
  const projection = launchProjection(JSON.parse(input.offeredText));
  return {
    review: launchReview(store, projection).map(({ path, scope, kind, approvable }) => ({ path, scope, kind, approvable })),
    effective: effectiveLaunch(store, projection),
    restrictedRefused: held !== null && launchReview(approvalsFrom(held), projection).length > 0,
  };
}
