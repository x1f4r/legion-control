// The version floor: the oldest agent this machine may be replaced by.
//
// A signature says a bundle came from the release key. It does not say the
// bundle is one anybody still wants: every release ever signed stays valid, so
// without a floor a restricted key could pipe an old public release to
// `self-update --stdin`, and that bundle's own bin/dispatch.sh and launcher.mjs
// would put back whatever a later release had fixed in them. --rollback was a
// second road to the same place.
//
// So each signed release declares in its package.json the oldest agent it may
// be replaced by (`legionControl.agentFloor`, at most its own version), and the
// agent keeps the highest floor it has committed in <base>/agent-floor.json.
// That file sits outside the tree an update replaces, which is the point: going
// back to an older tree cannot take the floor back with it. The floor in effect
// is the higher of the file and what the live tree declares (contract section
// 13).
//
// The file moves in three ways only. A committed install raises it, whether
// this agent made the install or an installer's promote.mjs did. A
// deliberate --allow-downgrade from an unrestricted session lowers it to its
// target just before the swap, and a swap that then fails without committing
// puts the earlier file back. Recovery from an interrupted swap never reads it:
// the tree recovery restores was live when the swap began.

import fs from 'node:fs';
import path from 'node:path';
import { swapPaths } from './agent-swap.mjs';
import { AGENT_TREE, AGENT_VERSION, basePath } from './config.mjs';
import { REASON } from './contract.mjs';
import { writePrivateAtomic } from './state.mjs';

/** A floor, recorded or declared, is exactly x.y.z. */
const FLOOR_PATTERN = /^[0-9]+\.[0-9]+\.[0-9]+$/;
/** Versions compare numerically on their leading x.y.z. */
const VERSION_PREFIX = /^([0-9]+)\.([0-9]+)\.([0-9]+)/;
/** The file is one small object; anything larger is not one this agent wrote. */
const MAX_FLOOR_BYTES = 64 * 1024;

export function floorPath(base = basePath()) {
  return path.join(base, 'agent-floor.json');
}

/** `[major, minor, patch]`, or null for anything that does not start with x.y.z. */
export function parseVersion(text) {
  const match = typeof text === 'string' ? VERSION_PREFIX.exec(text) : null;
  return match ? match.slice(1, 4).map(Number) : null;
}

/** Negative, zero or positive as `a` is older than, equal to or newer than `b`; null when either is not a version. */
export function compareVersions(a, b) {
  const [left, right] = [parseVersion(a), parseVersion(b)];
  if (!left || !right) return null;
  const index = left.findIndex((part, at) => part !== right[at]);
  return index === -1 ? 0 : left[index] - right[index];
}

/** Whether `a` is a version older than `b`. A value that is not a version is never older. */
export function isOlder(a, b) {
  const order = compareVersions(a, b);
  return order !== null && order < 0;
}

export function isFloorVersion(value) {
  return typeof value === 'string' && FLOOR_PATTERN.test(value);
}

function higher(a, b) {
  if (a === null) return b;
  if (b === null) return a;
  return isOlder(a, b) ? b : a;
}

/** What a tree's package.json says about itself: its version and the floor it declares, each null when it says none. */
export function treeDeclaration(dir) {
  try {
    const parsed = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
    const floor = parsed?.legionControl?.agentFloor;
    return { version: typeof parsed?.version === 'string' ? parsed.version : null, floor: isFloorVersion(floor) ? floor : null };
  } catch {
    return { version: null, floor: null };
  }
}

/**
 * The live tree under `base`, or the tree this agent runs from when there is
 * none there: a source checkout, or a base no installer has used yet.
 */
export function liveDeclaration(base = basePath()) {
  const live = treeDeclaration(swapPaths(base).live);
  return live.version !== null ? live : { version: AGENT_VERSION, floor: treeDeclaration(AGENT_TREE).floor };
}

/**
 * The floor file, as `missing`, `ok` with its version, or `invalid`: there is
 * a file and the floor it holds cannot be read, so nobody knows what it was.
 * `bytes` is what an invalid file held, so a failed downgrade can put it back.
 */
export function readFloorFile(base = basePath()) {
  const file = floorPath(base);
  let bytes;
  try {
    const stat = fs.lstatSync(file);
    if (!stat.isFile() || stat.size > MAX_FLOOR_BYTES) {
      return { state: 'invalid', version: null, bytes: null, error: `${file} is not a small regular file` };
    }
    bytes = fs.readFileSync(file);
  } catch (error) {
    if (error?.code === 'ENOENT') return { state: 'missing', version: null, bytes: null, error: null };
    return { state: 'invalid', version: null, bytes: null, error: `${file} cannot be read: ${error.message}` };
  }
  try {
    const parsed = JSON.parse(bytes.toString('utf8'));
    if (parsed?.schema === 1 && isFloorVersion(parsed.version)) return { state: 'ok', version: parsed.version, bytes, error: null };
  } catch {
    /* said below, without echoing the text */
  }
  return { state: 'invalid', version: null, bytes, error: `${file} does not hold {"schema":1,"version":"x.y.z"}` };
}

/**
 * The floor in effect: the higher of the file and the live tree's declaration.
 *
 * `unreadable` says the file exists but cannot be read. `version` is then only
 * what the live tree declares, which may be lower than the floor that was lost,
 * so nothing that would go below a floor treats it as the whole answer.
 */
export function effectiveFloor(base = basePath()) {
  const file = readFloorFile(base);
  const live = liveDeclaration(base);
  return {
    version: higher(file.state === 'ok' ? file.version : null, live.floor),
    unreadable: file.state === 'invalid',
    live: live.version,
    error: file.error,
  };
}

/**
 * The floor a reply states, as `status.agent.floor` and on every self-update
 * reply: null when this agent can state none, because the file cannot be read,
 * or because there is no file and the live tree declares no floor (a 2.x or
 * 3.0.x tree under an agent bootstrapping over it).
 */
export function reportedFloor(base = basePath()) {
  const floor = effectiveFloor(base);
  return floor.unreadable ? null : floor.version;
}

/**
 * Whether a session may move the agent to `target` (contract section 13).
 *
 * `mode` is `install` for --stdin, --from and --install, which goes below
 * neither the live version nor the floor without --allow-downgrade, or
 * `rollback`, which goes back by design and is held to the floor alone. A
 * rollback whose floor file cannot be read is held back as well: a floor
 * nobody can read is not permission to go below it.
 *
 * A restricted session only ever moves forward. An unrestricted one goes back
 * with --allow-downgrade, and `lowerFloor` then says the floor has to be set to
 * the target before the swap, so the launcher runs what was asked for.
 */
export function judgeAgentTransition({ mode, target, live = null, floor = null, floorUnreadable = false, restricted = false, allowDowngrade = false }) {
  const belowFloor = isOlder(target, floor);
  const belowLive = mode === 'install' && isOlder(target, live);
  const floorUnknown = mode === 'rollback' && floorUnreadable;
  if (!belowFloor && !belowLive && !floorUnknown) return { ok: true, lowerFloor: false };
  if (allowDowngrade && !restricted) return { ok: true, lowerFloor: belowFloor || floorUnreadable };

  const refuse = (reasonCode, message) => ({ ok: false, reasonCode, message });
  const unknown = `agent ${target} cannot be checked against the version floor because agent-floor.json cannot be read`;
  if (restricted) {
    if (!belowFloor && !belowLive) return refuse(REASON.restricted, `${unknown}; a restricted key can only move the agent forward until it can`);
    const back = floor ? `roll back to a signed agent at or above ${floor}` : 'roll back to the signed agent it retained';
    return refuse(REASON.restricted, `agent ${target} is below ${belowFloor ? `the version floor ${floor}` : `the installed ${live}`}; a restricted key can only move the agent forward or ${back}`);
  }
  const again = 'Repeat with --allow-downgrade from an unrestricted session to go back deliberately';
  if (!belowFloor && !belowLive) return refuse(REASON.badArgument, `${unknown}; nothing was staged. ${again}, which records ${target} as the floor`);
  return refuse(REASON.badArgument, `agent ${target} is older than ${belowFloor ? `the floor ${floor}` : `the installed ${live}`}; nothing was staged. ${again}`);
}

/** Write the floor file: a 0600 temp file, fsync, rename, then the directory fsync. */
export function writeFloor(base, version, reason) {
  const record = { schema: 1, version, updatedAt: new Date().toISOString(), reason };
  return writePrivateAtomic(floorPath(base), `${JSON.stringify(record, null, 2)}\n`);
}

/**
 * After a committed install: the floor becomes the highest of the file and the
 * floors given, and never moves down. A file that cannot be read is left
 * exactly as it is, because writing over it could lower a floor nobody can see.
 */
export function raiseFloor(base, declared) {
  const file = readFloorFile(base);
  if (file.state === 'invalid') return { ok: false, raised: false, version: null, error: `${file.error}, so the version floor was not raised` };
  const current = file.state === 'ok' ? file.version : null;
  const version = declared.filter(isFloorVersion).reduce(higher, current);
  if (version === current) return { ok: true, raised: false, version, error: null };
  const written = writeFloor(base, version, 'install');
  return written.ok ? { ok: true, raised: true, version, error: null } : { ok: false, raised: false, version: current, error: written.error };
}

/**
 * Before a deliberate downgrade's swap: set the floor to the target. `previous`
 * is what the file was, for restoreFloor.
 */
export function lowerFloor(base, version) {
  const previous = readFloorFile(base);
  return { ...writeFloor(base, version, 'downgrade'), previous };
}

/** Put back the floor file lowerFloor found, after a swap that did not commit. */
export function restoreFloor(base, previous) {
  const file = floorPath(base);
  if (previous.state === 'ok') return writeFloor(base, previous.version, 'restore');
  if (previous.state === 'invalid' && Buffer.isBuffer(previous.bytes)) return writePrivateAtomic(file, previous.bytes);
  if (previous.state === 'missing') {
    try {
      fs.rmSync(file, { force: true });
      return { ok: true, error: null };
    } catch (error) {
      return { ok: false, error: `${file} could not be removed: ${error.message}` };
    }
  }
  return { ok: false, error: `${file} could not be read before the downgrade, so it cannot be put back` };
}
