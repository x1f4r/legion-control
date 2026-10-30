// Launch values: what a setup document makes a device run (contract section 9.9).
//
// The shared setup carries the argv each client runs over ssh with its own
// key, and lineage proves only that a document claims an ancestor, never who
// wrote it. A restricted key may run `config set`, so before 3.1.0 a stolen
// phone could publish a descendant whose `agent` was `sh -c …`, and every peer
// adopted it and ran it on every machine with an unrestricted key (T1).
//
// The boundary is on the clients: each device runs only the launch values it
// approved itself. This file is the agent's part, defence in depth: in a
// restricted session `config set` refuses a document carrying a launch value
// the machine does not already hold (section 9.4). The held document's own
// launch values are the approved ones, so a restricted key can rename,
// reroute, reorder and reuse what is there, and cannot introduce anything.
//
// It implements the same rules as contract/tools/launch.mjs, written apart
// from it, and contract/vectors/launch.json holds the two to each other. Pure:
// no file system, no clock, no environment. Values compare as JSON, strings by
// code point with no case folding and no path or Unicode normalisation,
// because two spellings a person would call one path are two files to a file
// system that does not normalise.

import { isRepoName } from './setup-document.mjs';

/** The repository a client updates from when the setup names none. */
export const DEFAULT_REPO = 'x1f4r/legion-control';

/** The four grammars of section 15.2. */
const GRAMMARS = ['posix', 'dispatch', 'cmd', 'powershell'];

const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const own = (object, key) => (isPlainObject(object) && Object.hasOwn(object, key) ? object[key] : undefined);
// Records are keyed by ids from the document; defineProperty keeps an id such
// as "__proto__" an ordinary key rather than a prototype assignment.
const put = (object, key, value) => Object.defineProperty(object, key, { value, enumerable: true, writable: true, configurable: true });

const systemPath = (machine, system) => `machines[${machine}].systems[${system}]`;
const identityFilePath = (machine) => `machines[${machine}].ssh.identityFile`;
const REPO_PATH = 'appUpdates.githubRepo';
const LOCAL_PATH = 'local';

/**
 * Section 15.1: how a system's command line is quoted. Restricted on the route
 * means the dispatcher, whose agent is exactly interpreter and entry; otherwise
 * the shell written out, or cmd on Windows and posix anywhere else.
 */
export function resolveGrammar({ platform, shell, restricted, restrictedSession = false, agentLength }) {
  if (restricted === true || restrictedSession === true) {
    return agentLength === 2 ? { grammar: 'dispatch', refusal: null } : { grammar: null, refusal: 'restricted-agent-shape' };
  }
  if (shell === undefined || shell === null) return { grammar: platform === 'windows' ? 'cmd' : 'posix', refusal: null };
  if (shell === 'posix' || shell === 'cmd' || shell === 'powershell') return { grammar: shell, refusal: null };
  return { grammar: null, refusal: 'unknown-shell' };
}

// Control characters, C1 controls, zero-width and bidirectional formatting,
// word joiners and the byte order mark: each can make what a person approves
// look different from what runs.
const LAUNCH_TEXT = /^[^\u0000-\u001F\u007F-\u009F\u200B-\u200F\u202A-\u202E\u2060-\u2064\u2066-\u2069\uFEFF]+$/u;

/** Whether an argv item, an identityFile or local.agent can be shown to a person and approved. */
export function isLaunchText(text) {
  if (typeof text !== 'string' || /[\n\r]/.test(text)) return false;
  const size = Buffer.byteLength(text, 'utf8');
  return size >= 1 && size <= 4096 && LAUNCH_TEXT.test(text);
}

const isTuple = (value) => isPlainObject(value) && Array.isArray(value.agent) && value.agent.length > 0
  && value.agent.every((item) => typeof item === 'string') && typeof value.grammar === 'string';
const sameTuple = (a, b) => a.grammar === b.grammar && a.agent.length === b.agent.length
  && a.agent.every((item, index) => item === b.agent[index]);

/**
 * A system's `{ agent, grammar }`. An `agent` that is not a non-empty array of
 * strings runs nothing, so the tuple is inert: `{ agent: null, grammar: null }`.
 * The grammar is recorded rather than platform, shell and restricted, so an
 * editor writing `shell: "cmd"` where it was absent on Windows changes nothing
 * that runs; a grammar section 15.1 refuses is `refused:<code>`.
 */
function tupleOf(system) {
  const { agent } = system;
  if (!Array.isArray(agent) || agent.length === 0 || !agent.every((item) => typeof item === 'string')) return { agent: null, grammar: null };
  const resolved = resolveGrammar({
    platform: system.platform, shell: system.shell, restricted: system.restricted, restrictedSession: false, agentLength: agent.length,
  });
  return { agent: [...agent], grammar: resolved.grammar ?? `refused:${resolved.refusal}` };
}

/**
 * The launch values of a parsed document, read from the JSON itself and never
 * through a model that fills in defaults: `{ machines: [{ id, systems: [{ id,
 * tuple }], identityFile }], repo, local }`. Machines and systems are walked in
 * order, and a non-object, an item without a string id and every repeat of an
 * id after its first are skipped. `identityFile` is null when absent; `repo`
 * null is the built-in repository; `local` null is none, and `{ agent: null }`
 * is the default path.
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

/**
 * The approval record (section 11) that approves every launch value of
 * `document`. An inert tuple runs nothing and is accepted anywhere, so it is
 * not recorded. This is what the restricted check holds a push to: the values
 * the machine already has.
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

/**
 * Whether `offered` is the launcher migration of `approved`: the same grammar
 * and argv except one item, which moves from `<base>/agent/src/index.mjs` to
 * `<base>/bin/launcher.mjs` under the same non-empty base with no `..`
 * segment, compared with `\` read as `/`, since one client writes the new path
 * with forward slashes into a Windows argv. The 3.1 upgrade moves every system
 * this way, to a file the agent installs itself under a base already approved.
 * The reverse is never a migration.
 */
export function isLauncherMigration(approved, offered) {
  if (!isTuple(approved) || !isTuple(offered)) return false;
  if (approved.grammar !== offered.grammar || approved.agent.length !== offered.agent.length) return false;
  const differing = approved.agent.flatMap((item, index) => (item === offered.agent[index] ? [] : [index]));
  if (differing.length !== 1) return false;
  const entry = '/agent/src/index.mjs';
  const launcher = '/bin/launcher.mjs';
  const from = approved.agent[differing[0]].replaceAll('\\', '/');
  const to = offered.agent[differing[0]].replaceAll('\\', '/');
  if (!from.endsWith(entry) || !to.endsWith(launcher)) return false;
  const base = from.slice(0, -entry.length);
  return base.length > 0 && base === to.slice(0, -launcher.length) && !base.split('/').includes('..');
}

/** Every value of a record wherever it sits: a value approved anywhere is accepted anywhere. */
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
  return { tuples, identityFiles, repo: typeof repo === 'string' ? repo : null, local: isPlainObject(local) ? local : null };
}

const runnableTuple = (tuple) => isTuple(tuple) && tuple.agent.every(isLaunchText) && GRAMMARS.includes(tuple.grammar);

/**
 * One entry for each offered launch value the record does not accept without
 * asking, in this order: for each machine its systems and then its
 * identityFile, then the repository, then local. `{ path, scope, kind,
 * approvable }`: `kind` is `changed` when the record holds a value at that
 * path, and `approvable` is false for a value that could only be reverted: an
 * argv item, identityFile or local.agent that fails isLaunchText, a refused
 * grammar, a repository that is not owner/name.
 */
export function launchReview(store, projection) {
  const approved = approvedValues(store);
  const recordedTuple = (machine, system) => own(own(own(own(store, 'machines'), machine), 'systems'), system);
  const recordedIdentityFile = (machine) => own(own(own(store, 'machines'), machine), 'identityFile');
  const entries = [];
  const entry = (path, scope, recorded, approvable) => entries.push({ path, scope, kind: recorded === undefined ? 'new' : 'changed', approvable });

  for (const machine of projection.machines) {
    for (const system of machine.systems) {
      const { tuple } = system;
      const accepted = tuple.agent === null
        || approved.tuples.some((candidate) => sameTuple(candidate, tuple) || isLauncherMigration(candidate, tuple));
      if (!accepted) entry(systemPath(machine.id, system.id), 'system', recordedTuple(machine.id, system.id), runnableTuple(tuple));
    }
    if (machine.identityFile !== null && !approved.identityFiles.includes(machine.identityFile)) {
      entry(identityFilePath(machine.id), 'identityFile', recordedIdentityFile(machine.id), isLaunchText(machine.identityFile));
    }
  }
  if (projection.repo !== null && approved.repo !== projection.repo) {
    entry(REPO_PATH, 'repo', own(store, 'githubRepo'), isRepoName(projection.repo));
  }
  const { local } = projection;
  if (local !== null && local.agent !== null && !(approved.local !== null && approved.local.agent === local.agent)) {
    entry(LOCAL_PATH, 'local', own(store, 'local'), isLaunchText(local.agent));
  }
  return entries;
}
