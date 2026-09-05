// Writes contract/fixtures/*.json and contract/fixtures/index.json.
//
// The fixtures are generated rather than hand written so that the facts shared
// between them stay shared: an operation id means the same operation in the
// status that lists it, in the reply that started it and in the history that
// remembers it, and the 2.x compatibility keys in a status are computed from
// the first service instead of being copied by hand and drifting. Everything
// here is written as one machine on one afternoon, because a fixture set with
// four unrelated clocks in it reads as noise.
//
// The machine is a Raspberry Pi called atlas with three services on it: a T3
// server watched through its state database, a demo service watched through a
// command, and a service nobody has described a busy probe for. That last one
// is not padding: "nobody said" is a state the gate has to get right, and it is
// the state a real machine spends most of its life in.
//
// Run: node contract/tools/make-fixtures.mjs

import fs from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { canonicalHash } from './canonical.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, '..', 'fixtures');

const AGENT = '3.0.0';
const LINUX = { id: 'linux', name: 'Raspberry Pi OS' };
const MAC = { id: 'mac', name: 'macOS' };
const WINDOWS = { id: 'windows', name: 'Windows 11' };

// One afternoon, in order.
const T = (minutes, seconds = 0, millis = 0) => {
  const base = Date.UTC(2026, 8, 5, 14, 0, 0, 0);
  return new Date(base + minutes * 60000 + seconds * 1000 + millis).toISOString();
};

const OPS = {
  updateRunning: '6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3',
  updateDone: 'b2e9d5c1-77a4-4f0b-8d36-1e5a9c02f4d8',
  restartQueued: '3a7f18b0-92c6-4d51-b0ea-6f2c8d47e915',
  boot: '0d4c6e21-5a83-4b7f-9c10-2e8b3f5a6d94',
  cycle: '7c5b3d92-4e18-4a6f-b273-9d0e1f8a5c46',
  interrupted: 'e81a4f37-6c25-4d90-a1b8-53f7c2e6094d',
  rolledBack: '9b6d0e54-1f83-4c27-8a95-4e2b7d13f6a0',
  sleep: '5e2a9c76-8d34-4b1e-97f0-6a3d5b8c214f',
  run: 'a1f8c3d7-2b46-4e58-90ca-7d1b4f62e830',
  selfRollback: '7fd6806f-682a-4eb4-9aa7-a0d0c9fda733',
  selfUpdate: '4d9e7a12-6f35-4c80-b1e7-8a03c5d29ف'.replace(/[^a-z0-9-]/g, '') + 'b6',
  cancelled: '8c3a5f60-1d29-4b74-a8e3-5f0c7b91d642',
  expired: '2b7e4d18-9a63-4f52-8c07-3e1d6a45b980',
  postcondition: 'f60d2c85-4a17-4b39-92e1-7c8b03d5f4a6',
  applyFailed: 'd35b8e04-7c91-4a26-b5f8-0e3a7d192c64',
  latestUnknown: '1e9f4b73-8d02-4c65-a397-6b5e0d8c2ف14'.replace(/[^a-z0-9-]/g, '') + 'a7',
};

// One setup, and the chain of documents it has been through. The hashes are the
// canonical hashes of real documents from contract/hash-vectors.json, so a
// client test can canonicalise the document fixture below and land on H_CURRENT
// rather than on a number somebody typed.
const SETUP_ID = 'setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846';
const H_PARENT = '1c680fab2ecc79f4b1c2a06e4e3ed2f8a1d54c6f0b93e7a25c8d0f416b2e93a7';
const H_GRANDPARENT = '1a5ee25ee34b2f9c07d61a8b45e0c3f27d9b8140a6532e7fc019d84b6a3e5027';
const H_OTHER_BRANCH = '4a88b8efec1592c0d3b7615ae820f4c96d1e037b58a2c94f6013e7d58b2a06cf';
const LINEAGE = [H_PARENT, H_GRANDPARENT];

// The shared setup document, defined before anything that reports a hash for it.
//
// H_CURRENT is COMPUTED from these exact bytes with the reference
// implementation in contract/tools/canonical.mjs, so the number the config
// fixtures report really is the hash of controller-document.sites-helpers.json.
// A client can canonicalise that file with its own implementation and land on
// the same value; a hand-typed constant here would have proved nothing.
//
// Two of the sites carry the SAME private subnet on purpose. That is what two
// homes behind stock routers look like, and it is the case a prefix match
// cannot decide.

const CONTROLLER_DOC = {
  version: 1,
  controller: {
    id: SETUP_ID,
    name: 'Home setup',
    revision: 12,
    updatedAt: T(-4200),
    source: 'mac',
    device: "Robert's MacBook Pro",
    lineage: LINEAGE,
  },
  sites: [
    { id: 'attic', name: 'Attic house', lanPrefixes: ['192.168.178.'], broadcast: ['192.168.178.255'] },
    { id: 'flat', name: 'New flat', lanPrefixes: ['192.168.178.'], broadcast: ['192.168.178.255'] },
  ],
  machines: [
    {
      id: 'pi',
      name: 'Atlas',
      site: 'attic',
      alwaysOn: true,
      endpoints: [
        { id: 'lan', kind: 'lan', host: '192.168.178.40', user: 'x1f4r', port: 22 },
        { id: 'remote', kind: 'remote', host: 'atlas.example-tailnet.ts.net', user: 'x1f4r', port: 22 },
      ],
      systems: [
        { id: 'linux', name: 'Raspberry Pi OS', platform: 'linux', shell: 'posix', restricted: true, agent: ['node', '/home/x1f4r/.legion-control/agent/src/index.mjs'] },
      ],
    },
    {
      id: 'legion',
      name: 'Legion 7',
      site: 'attic',
      alwaysOn: false,
      endpoints: [
        { id: 'lan-linux', kind: 'lan', host: '192.168.178.41', user: 'x1f4r', port: 22, system: 'linux' },
        { id: 'remote-linux', kind: 'remote', host: 'legion-cachy.example-tailnet.ts.net', user: 'x1f4r', port: 22, system: 'linux' },
        { id: 'remote-windows', kind: 'remote', host: 'legion-win.example-tailnet.ts.net', user: 'x1f4r', port: 22, system: 'windows' },
      ],
      wake: {
        mac: 'AA:BB:CC:DD:EE:FF',
        broadcast: ['192.168.178.255'],
        ports: [9, 7],
        lanPrefix: '192.168.178.',
        // helpers is the ordered list; helper repeats the first entry so a 1.2
        // client still finds the one helper it knows how to read. A 1.2 client
        // does not do failover, and nothing here pretends it does.
        helper: { machine: 'pi', action: 'wake-legion' },
        helpers: [
          { machine: 'pi', action: 'wake-legion' },
          { machine: 'nas', action: 'wake-legion' },
        ],
      },
      systems: [
        { id: 'linux', name: 'CachyOS', platform: 'linux', shell: 'posix', agent: ['node', '/home/x1f4r/.legion-control/agent/src/index.mjs'] },
        { id: 'windows', name: 'Windows 11', platform: 'windows', shell: 'powershell', agent: ['node', 'C:/Users/x1f4r/.legion-control/agent/src/index.mjs'] },
      ],
    },
    {
      id: 'nas',
      name: 'Attic NAS',
      site: 'attic',
      alwaysOn: true,
      endpoints: [{ id: 'lan', kind: 'lan', host: '192.168.178.10', user: 'x1f4r', port: 22 }],
      systems: [
        { id: 'linux', name: 'Debian', platform: 'linux', shell: 'posix', agent: ['node', '/home/x1f4r/.legion-control/agent/src/index.mjs'] },
      ],
    },
    {
      id: 'tower',
      name: 'Tower',
      site: 'flat',
      alwaysOn: false,
      endpoints: [
        { id: 'lan', kind: 'lan', host: '192.168.178.20', user: 'x1f4r', port: 22 },
        { id: 'remote', kind: 'remote', host: 'tower.example-tailnet.ts.net', user: 'x1f4r', port: 22 },
      ],
      wake: {
        mac: '11:22:33:44:55:66',
        broadcast: ['192.168.178.255'],
      },
      systems: [
        { id: 'linux', name: 'CachyOS', platform: 'linux', shell: 'posix', agent: ['node', '/home/x1f4r/.legion-control/agent/src/index.mjs'] },
        { id: 'windows', name: 'Windows 11', platform: 'windows', shell: 'cmd', agent: ['node', 'C:/Users/x1f4r/.legion-control/agent/src/index.mjs'] },
      ],
    },
  ],
  appUpdates: { githubRepo: 'x1f4r/legion-control' },
};

/** Exactly the bytes add() writes, so the hash describes the file on disk. */
function fixtureBytes(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

const CONTROLLER_DOC_BYTES = fixtureBytes(CONTROLLER_DOC);
const H_CURRENT = canonicalHash(CONTROLLER_DOC_BYTES);
const CONTROLLER_DOC_SIZE = Buffer.byteLength(CONTROLLER_DOC_BYTES, 'utf8');

const INITIATOR = {
  mac: { client: 'mac', device: 'roberts-macbook-pro', user: 'x1f4r' },
  phone: { client: 'phone', device: 'pixel-8', user: 'x1f4r' },
  desktop: { client: 'desktop', device: 'legion-cachy', user: 'x1f4r' },
  scheduler: { client: 'cli', device: null, user: 'x1f4r' },
};

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

function envelope({ ok = true, system = LINUX, reasonCode, message, notes } = {}) {
  const reply = { ok, contract: 3, agentVersion: AGENT, system };
  if (reasonCode !== undefined) reply.reasonCode = reasonCode;
  if (message !== undefined) reply.message = message;
  if (notes !== undefined) reply.notes = notes;
  return reply;
}

function busy({
  busy: isBusy,
  unknown = false,
  monitored = true,
  reason,
  evidence,
  checkedAt,
  elapsedMs,
  error = null,
  counts,
  threads,
  threadsTruncated,
}) {
  const value = { busy: isBusy, unknown, monitored, reason, evidence, checkedAt, elapsedMs, error };
  if (counts) Object.assign(value, counts);
  if (threads) {
    value.threads = threads;
    value.threadsTruncated = threadsTruncated ?? 0;
  }
  return value;
}

const T3_THREAD = {
  threadId: 'thr_01JT8Q4V2M',
  turnId: 'trn_01JT8Q7C9X',
  title: 'Rework the busy gate',
  state: 'running',
  at: T(-6),
  blocking: true,
  disposition: 'running',
  stale: false,
};

const T3_STALE_THREAD = {
  threadId: 'thr_01JQ2H9B4K',
  turnId: 'trn_01JQ2H9B7P',
  title: 'Import the old exports',
  state: 'pending',
  at: T(-4320),
  blocking: false,
  disposition: 'stale',
  stale: true,
};

const BUSY_T3 = busy({
  busy: true,
  reason: '1 turn running',
  evidence: 't3-sqlite',
  checkedAt: T(0),
  elapsedMs: 118,
  counts: { runningTurns: 1, pendingTurns: 0, pendingApprovals: 0, staleTurns: 1, staleApprovals: 0 },
  threads: [T3_THREAD, T3_STALE_THREAD],
});

const BUSY_T3_IDLE = busy({
  busy: false,
  reason: 'idle',
  evidence: 't3-sqlite',
  checkedAt: T(0),
  elapsedMs: 96,
  counts: { runningTurns: 0, pendingTurns: 0, pendingApprovals: 0, staleTurns: 1, staleApprovals: 0 },
  threads: [T3_STALE_THREAD],
});

const BUSY_DEMO_IDLE = busy({
  busy: false,
  reason: 'idle',
  evidence: 'command',
  checkedAt: T(0),
  elapsedMs: 41,
});

const BUSY_DECLARED_NONE = busy({
  busy: false,
  reason: 'declared never busy',
  evidence: 'none',
  checkedAt: T(0),
  elapsedMs: 0,
});

const BUSY_UNMONITORED = busy({
  busy: true,
  unknown: true,
  monitored: false,
  reason: 'not monitored',
  evidence: 'unmonitored',
  checkedAt: T(0),
  elapsedMs: 0,
  error: 'no busy probe is configured for edge; disruptive work waits until one is, or until busy.type is set to "none"',
});

const BUSY_TIMED_OUT = busy({
  busy: true,
  unknown: true,
  reason: 'busy state unknown',
  evidence: 'timed-out',
  checkedAt: T(0),
  elapsedMs: 8000,
  error: 'the busy probe did not answer within 8000 ms',
});

const RELAY_UP = { configured: true, running: true };
const RELAY_NONE = { configured: false, running: false };

const T3_LAST_UPDATE = {
  at: T(-1440),
  from: '0.0.35-nightly.20260903',
  to: '0.0.36-nightly.20260904',
  result: 'updated',
  message: 'updated t3 from 0.0.35-nightly.20260903 to 0.0.36-nightly.20260904',
};

const WINDOWS_NIGHTLY = [{ days: ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'], from: '02:00', to: '06:00' }];

function serviceUpdates({
  automatic = true,
  inherited = true,
  pauseUntil = null,
  maintenanceWindows,
  eligibleNow,
  deferredReason = null,
}) {
  const value = { automatic, inherited, pauseUntil, eligibleNow, deferredReason };
  if (maintenanceWindows !== undefined) value.maintenanceWindows = maintenanceWindows;
  return value;
}

function service(overrides) {
  return {
    id: 'service',
    name: 'Service',
    kind: 'command',
    installed: null,
    latest: null,
    channel: null,
    upToDate: null,
    running: false,
    healthy: false,
    port: null,
    staged: null,
    appPath: null,
    busy: BUSY_DEMO_IDLE,
    relay: RELAY_NONE,
    pendingRestart: false,
    lastUpdate: null,
    canUpdate: true,
    canRestart: true,
    process: { running: false, state: 'inactive', startedAt: null, error: null },
    health: { ok: false, status: 0, checkedAt: null, elapsedMs: null, error: null },
    endpoint: { configured: false, reachable: null },
    updates: serviceUpdates({ eligibleNow: false }),
    pendingVersion: null,
    drain: false,
    lastOperation: null,
    ...overrides,
  };
}

const SERVICE_T3 = service({
  id: 't3',
  name: 'T3 Code',
  kind: 'npm',
  installed: '0.0.36-nightly.20260904',
  latest: '0.0.37-nightly.20260905',
  channel: 'nightly',
  upToDate: false,
  running: true,
  healthy: true,
  port: 8081,
  busy: BUSY_T3,
  relay: RELAY_UP,
  lastUpdate: T3_LAST_UPDATE,
  process: { running: true, state: 'active', startedAt: T(-2870), error: null },
  health: { ok: true, status: 200, checkedAt: T(0), elapsedMs: 8, error: null },
  endpoint: { configured: true, reachable: null },
  updates: serviceUpdates({ eligibleNow: false, deferredReason: 'busy' }),
  drain: true,
  lastOperation: { opId: OPS.updateDone, kind: 'update', action: 'updated', at: T(-1440) },
});

const SERVICE_DEMO = service({
  id: 'demo',
  name: 'Demo',
  installed: '1.4.2',
  latest: '1.4.2',
  upToDate: true,
  running: true,
  healthy: true,
  port: 8765,
  process: { running: true, state: 'active', startedAt: T(-2865), error: null },
  health: { ok: true, status: 200, checkedAt: T(0), elapsedMs: 3, error: null },
  updates: serviceUpdates({ automatic: false, inherited: false, eligibleNow: false, deferredReason: 'policy-off' }),
  lastOperation: { opId: OPS.rolledBack, kind: 'update', action: 'rolled-back', at: T(-2880) },
});

const SERVICE_EDGE = service({
  id: 'edge',
  name: 'Edge Proxy',
  installed: '2.9.0',
  latest: null,
  upToDate: null,
  running: true,
  healthy: true,
  port: 8443,
  busy: BUSY_UNMONITORED,
  process: { running: true, state: 'active', startedAt: T(-2860), error: null },
  health: { ok: true, status: 200, checkedAt: T(0), elapsedMs: 5, error: null },
  updates: serviceUpdates({ eligibleNow: false, deferredReason: 'busy-unknown' }),
  canUpdate: false,
});

function legacyBlock(first) {
  if (!first) return null;
  const block = {
    installed: first.installed,
    nightly: first.latest,
    upToDate: first.upToDate,
    serverRunning: first.running,
    healthy: first.healthy,
    port: first.port,
  };
  if (first.kind === 'app') {
    block.staged = first.staged;
    block.appPath = first.appPath;
  }
  return block;
}

// ---------------------------------------------------------------------------
// Operation records
// ---------------------------------------------------------------------------

function record(overrides) {
  return {
    id: 'unset',
    kind: 'update',
    service: null,
    target: null,
    actionId: null,
    mode: 'manual',
    initiator: INITIATOR.mac,
    state: 'running',
    phase: 'installing',
    progress: null,
    requestedAt: T(0),
    startedAt: T(0),
    updatedAt: T(0),
    finishedAt: null,
    expiresAt: null,
    pid: 24817,
    detached: true,
    result: null,
    log: [],
    agentVersion: AGENT,
    systemId: 'linux',
    ...overrides,
  };
}

function summarise(op, { action = null, reasonCode = null } = {}) {
  return {
    id: op.id,
    kind: op.kind,
    service: op.service,
    target: op.target,
    actionId: op.actionId,
    mode: op.mode,
    state: op.state,
    phase: op.phase,
    action: op.result ? op.result.action : action,
    reasonCode: op.result ? op.result.reasonCode : reasonCode,
    requestedAt: op.requestedAt,
    updatedAt: op.updatedAt,
    expiresAt: op.expiresAt,
  };
}

const OP_UPDATE_RUNNING = record({
  id: OPS.updateRunning,
  kind: 'update',
  service: 't3',
  mode: 'force',
  state: 'running',
  phase: 'installing',
  progress: { step: 4, of: 8, note: 'npm install -g t3@0.0.37-nightly.20260905' },
  requestedAt: T(0),
  startedAt: T(0, 1),
  updatedAt: T(0, 47),
  from: '0.0.36-nightly.20260904',
  to: '0.0.37-nightly.20260905',
  log: [
    { at: T(0, 1), line: 'lock taken for update t3' },
    { at: T(0, 2), line: 'busy gate forced past: 1 turn running' },
    { at: T(0, 9), line: 'stopped t3-code.service' },
    { at: T(0, 11), line: 'npm install -g t3@0.0.37-nightly.20260905' },
  ],
});

const OP_UPDATE_DONE = record({
  id: OPS.updateDone,
  kind: 'update',
  service: 't3',
  mode: 'scheduled',
  initiator: INITIATOR.scheduler,
  state: 'finished',
  phase: 'done',
  progress: { step: 8, of: 8, note: 't3 answered on 8081' },
  requestedAt: T(-1441),
  startedAt: T(-1441, 1),
  updatedAt: T(-1440),
  finishedAt: T(-1440),
  pid: null,
  detached: false,
  from: '0.0.35-nightly.20260903',
  to: '0.0.36-nightly.20260904',
  result: {
    ok: true,
    action: 'updated',
    reasonCode: null,
    message: 'updated t3 from 0.0.35-nightly.20260903 to 0.0.36-nightly.20260904',
    from: '0.0.35-nightly.20260903',
    to: '0.0.36-nightly.20260904',
    exitCode: 0,
    output: null,
  },
  log: [
    { at: T(-1441, 1), line: 'lock taken for update t3' },
    { at: T(-1441, 4), line: 'idle: 0 turns running' },
    { at: T(-1441, 9), line: 'stopped t3-code.service' },
    { at: T(-1440, -40), line: 'installed 0.0.36-nightly.20260904' },
    { at: T(-1440), line: 'started t3-code.service, health 200' },
  ],
});

const OP_RESTART_QUEUED = record({
  id: OPS.restartQueued,
  kind: 'restart',
  service: 't3',
  mode: 'queued',
  initiator: INITIATOR.phone,
  state: 'queued',
  phase: 'queued',
  requestedAt: T(-3),
  startedAt: null,
  updatedAt: T(-3),
  expiresAt: T(237),
  pid: null,
  detached: false,
  log: [{ at: T(-3), line: 'queued until idle, expires in 4h' }],
});

const OP_BOOT_REBOOTED = record({
  id: OPS.boot,
  kind: 'boot',
  target: 'windows',
  mode: 'manual',
  initiator: INITIATOR.phone,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-2880),
  startedAt: T(-2880),
  updatedAt: T(-2874),
  finishedAt: T(-2874),
  pid: null,
  detached: false,
  systemId: 'linux',
  result: {
    ok: true,
    action: 'rebooted',
    reasonCode: null,
    message: 'the machine came back on Windows 11, booted at 2026-09-03T14:07:00.000Z, which is after the reboot was ordered',
    exitCode: 0,
    output: null,
    verified: true,
  },
  log: [
    { at: T(-2880), line: 'BootNext set to 0002 and read back' },
    { at: T(-2880, 3), line: 'reboot scheduled' },
    { at: T(-2874), line: 'recovery: uptime 42s and the running system is windows, so the transition happened' },
  ],
});

const OP_CYCLE = record({
  id: OPS.cycle,
  kind: 'cycle',
  mode: 'scheduled',
  initiator: INITIATOR.scheduler,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-720),
  startedAt: T(-720),
  updatedAt: T(-718),
  finishedAt: T(-718),
  pid: null,
  detached: false,
  result: {
    ok: true,
    action: 'cycled',
    reasonCode: null,
    message: '3 services walked: 1 updated, 2 deferred',
    exitCode: 0,
    output: null,
  },
  children: [
    { opId: OPS.updateDone, service: 't3', action: 'updated', reasonCode: null },
    { opId: OPS.applyFailed, service: 'demo', action: 'deferred', reasonCode: 'policy-off' },
    { opId: OPS.latestUnknown, service: 'edge', action: 'deferred', reasonCode: 'busy-unknown' },
  ],
  log: [
    { at: T(-720), line: 'lock taken for cycle' },
    { at: T(-720, 1), line: 'recovery: nothing to recover' },
    { at: T(-720, 2), line: 'queue: nothing to run' },
    { at: T(-718), line: 'cycle done' },
  ],
});

const OP_INTERRUPTED = record({
  id: OPS.interrupted,
  kind: 'update',
  service: 't3',
  mode: 'manual',
  initiator: INITIATOR.desktop,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-4320),
  startedAt: T(-4320),
  updatedAt: T(-4260),
  finishedAt: T(-4260),
  pid: null,
  from: '0.0.31-nightly.20260901',
  to: '0.0.32-nightly.20260902',
  result: {
    ok: false,
    action: 'interrupted',
    reasonCode: 'interrupted',
    message: 'the worker holding this operation is gone and the installed version is 0.0.31-nightly.20260901, so the install did not complete; t3 was started again and left on the version it had',
    from: '0.0.31-nightly.20260901',
    to: '0.0.31-nightly.20260901',
    exitCode: null,
    output: null,
  },
  log: [
    { at: T(-4320), line: 'lock taken for update t3' },
    { at: T(-4320, 8), line: 'stopped t3-code.service' },
    { at: T(-4260), line: 'recovery: pid 21044 is gone in phase installing' },
    { at: T(-4260), line: 'recovery: ensured t3-code.service is up' },
  ],
});

const OP_ROLLED_BACK = record({
  id: OPS.rolledBack,
  kind: 'update',
  service: 'demo',
  mode: 'manual',
  initiator: INITIATOR.mac,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-2881),
  startedAt: T(-2881),
  updatedAt: T(-2880),
  finishedAt: T(-2880),
  pid: null,
  detached: false,
  from: '1.4.2',
  to: '1.4.2',
  result: {
    ok: false,
    action: 'rolled-back',
    reasonCode: 'rolled-back',
    message: 'demo 1.5.0 would not answer its health check, so 1.4.2 was put back and is running',
    from: '1.4.2',
    to: '1.4.2',
    exitCode: 1,
    output: null,
  },
  log: [
    { at: T(-2881), line: 'installed 1.5.0' },
    { at: T(-2880, -30), line: 'health check failed 4 times: connection refused' },
    { at: T(-2880), line: 'rolled back to 1.4.2, health 200' },
  ],
});

const OP_RUN = record({
  id: OPS.run,
  kind: 'run',
  actionId: 'sunshine-restart',
  mode: 'manual',
  initiator: INITIATOR.phone,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-30),
  startedAt: T(-30),
  updatedAt: T(-30, 2),
  finishedAt: T(-30, 2),
  pid: null,
  detached: false,
  result: {
    ok: true,
    action: 'ran',
    reasonCode: null,
    message: 'sunshine-restart finished with exit code 0',
    exitCode: 0,
    output: 'Stopping sunshine.service\nStarting sunshine.service\nsunshine.service is active',
  },
  log: [{ at: T(-30), line: 'run sunshine-restart' }],
});

const OP_SLEEP = record({
  id: OPS.sleep,
  kind: 'sleep',
  mode: 'force',
  initiator: INITIATOR.mac,
  state: 'running',
  phase: 'suspending',
  requestedAt: T(-1),
  startedAt: T(-1),
  updatedAt: T(-1, 2),
  pid: 24902,
  detached: false,
  log: [
    { at: T(-1), line: 'lock taken for sleep' },
    { at: T(-1, 1), line: 'busy gate forced past: 1 turn running' },
    { at: T(-1, 2), line: 'systemctl suspend accepted' },
  ],
});

const OP_SELF_UPDATE = record({
  id: OPS.selfUpdate,
  kind: 'self-update',
  target: `install:${'1'.repeat(64)}`,
  mode: 'manual',
  initiator: INITIATOR.mac,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-60),
  startedAt: T(-60),
  updatedAt: T(-59),
  finishedAt: T(-59),
  pid: null,
  detached: false,
  from: '2.1.0',
  to: '3.0.0',
  result: {
    ok: true,
    action: 'installed',
    reasonCode: null,
    message: 'agent 3.0.0 is installed and 2.1.0 is kept at /home/x1f4r/.legion-control/agent.prev',
    from: '2.1.0',
    to: '3.0.0',
    exitCode: 0,
    output: null,
  },
  log: [
    { at: T(-60), line: 'manifest signature verified against the release key' },
    { at: T(-60, 4), line: '31 files hashed and matched' },
    { at: T(-59, -20), line: 'self-test: 18 modules loaded, contract 3' },
    { at: T(-59), line: 'agent.new swapped in, previous tree kept' },
  ],
});

const OP_CANCELLED = record({
  id: OPS.cancelled,
  kind: 'boot',
  target: 'windows',
  mode: 'queued',
  initiator: INITIATOR.phone,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-200),
  startedAt: null,
  updatedAt: T(-120),
  finishedAt: T(-120),
  pid: null,
  detached: false,
  expiresAt: T(40),
  result: {
    // The cancel succeeded; the boot did not happen. The reply's ok is about the
    // first, this one is about the second, and collapsing them would tell a
    // client the machine rebooted.
    ok: false,
    action: 'cancelled',
    reasonCode: 'cancelled',
    message: 'the queued boot into windows was cancelled before it ran',
    exitCode: null,
    output: null,
  },
  log: [
    { at: T(-200), line: 'queued until idle, expires in 4h' },
    { at: T(-120), line: 'cancelled by phone' },
  ],
});

const OP_POSTCONDITION = record({
  id: OPS.postcondition,
  kind: 'update',
  service: 'demo',
  mode: 'manual',
  initiator: INITIATOR.desktop,
  state: 'finished',
  phase: 'done',
  requestedAt: T(-90),
  startedAt: T(-90),
  updatedAt: T(-89),
  finishedAt: T(-89),
  pid: null,
  detached: false,
  from: '1.4.2',
  to: '1.4.2',
  result: {
    ok: false,
    action: 'failed',
    reasonCode: 'postcondition-failed',
    message: 'the update command exited 0 and demo is still on 1.4.2, not the 1.5.1 that was expected',
    from: '1.4.2',
    to: '1.4.2',
    exitCode: 0,
    output: 'nothing to do\n',
    verified: false,
  },
  log: [
    { at: T(-90), line: 'installed version before: 1.4.2' },
    { at: T(-89), line: 'installed version after: 1.4.2, expected 1.5.1' },
  ],
});

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

function status({
  system = LINUX,
  hostname = 'atlas',
  services = [SERVICE_T3, SERVICE_DEMO, SERVICE_EDGE],
  timing = { budgetMs: 20000, elapsedMs: 2310, partial: false },
  agent = {
    version: AGENT,
    contract: 3,
    base: '/home/x1f4r/.legion-control',
    node: '24.9.0',
    restrictedSession: false,
    capabilities: ['setup-lineage', 'wol-action', 'configured-telemetry', 'service-config-admin'],
  },
  config = { ok: true, source: 'file', problems: [] },
  automatic = true,
  pauseUntil = null,
  maintenanceWindows = WINDOWS_NIGHTLY,
  inWindowNow = false,
  nextWindow = T(720),
  lastCycle = { opId: OPS.cycle, at: T(-718), action: 'cycled' },
  operations = { running: [], queued: [], recent: [] },
  controller = {
    hash: H_CURRENT,
    id: SETUP_ID,
    revision: 12,
    updatedAt: T(-4200),
    source: 'mac',
  },
  bootTargets = [{ id: 'windows', name: 'Windows 11' }],
  actions = [
    { id: 'sunshine-restart', name: 'Restart Sunshine', confirm: null, busyGated: true, kind: 'command' },
    { id: 'flush-cache', name: 'Flush the build cache', confirm: 'This throws away every cached build.', busyGated: false, kind: 'command' },
    // A wake helper's action. The agent sends the magic packet itself, so this
    // Pi can wake the tower on its own LAN without a wakeonlan binary, and it
    // is not busy-gated because sending a UDP packet disturbs nothing.
    { id: 'wake-tower', name: 'Wake the tower', confirm: null, busyGated: false, kind: 'wol' },
  ],
  notes,
} = {}) {
  const first = services[0] ?? null;
  const monitoredServices = services.filter((entry) => entry.busy.monitored).length;
  const blocking = services.some((entry) => entry.busy.busy);
  const unknown = services.some((entry) => entry.busy.unknown);
  const reasons = services.filter((entry) => entry.busy.busy).map((entry) => `${entry.name}: ${entry.busy.reason}`);

  const reply = envelope({ system });
  Object.assign(reply, {
    os: system.id,
    hostname,
    timing,
    agent,
    config,
    services,
    busy: {
      busy: blocking,
      unknown,
      reason: reasons.length > 0 ? reasons.join('; ') : 'idle',
      monitoredServices,
      unmonitoredServices: services.length - monitoredServices,
    },
    bootTargets,
    actions,
    autoUpdate: automatic,
    updates: { automatic, pauseUntil, maintenanceWindows, inWindowNow, nextWindow, lastCycle },
    operations,
    controller,
    t3: legacyBlock(first),
    pendingRestart: first ? first.pendingRestart : false,
    lastUpdate: first ? first.lastUpdate : null,
    connect: first ? first.relay : RELAY_NONE,
  });
  if (notes) reply.notes = notes;
  return reply;
}

const fixtures = [];

function add(file, entry, data) {
  const isReply = entry.kind !== 'document';
  fixtures.push({
    file,
    kind: entry.kind ?? 'reply',
    expect: entry.expect ?? 'valid',
    command: isReply ? entry.command : null,
    variant: entry.variant,
    schema: entry.schema,
    ok: isReply ? data.ok : null,
    exitCode: isReply ? (data.ok ? 0 : 1) : null,
    contract: isReply ? (data.contract ?? null) : null,
    action: isReply ? (data.action ?? null) : null,
    reasonCode: isReply ? (data.reasonCode ?? null) : null,
    description: entry.description,
    tags: entry.tags ?? [],
  });
  fs.writeFileSync(path.join(OUT, file), `${JSON.stringify(data, null, 2)}\n`, 'utf8');
}

/**
 * A setup document or a bindings file: not a reply, and validated against a
 * schema that has no envelope. The invalid ones are as much of the contract as
 * the valid ones — a rule nothing ever fails is a rule nobody implemented.
 */
function addDocument(file, entry, data) {
  add(file, { ...entry, kind: 'document', command: null }, data);
}

/**
 * History, sorted and counted from the list rather than beside it.
 *
 * Written this way because the first attempt had a hand-written `returned` that
 * disagreed with the array and rows in the order they were thought of. Both are
 * exactly what a client would trip over, and neither is visible by reading.
 */
function addHistory(entry) {
  const data = entry.data;
  data.operations.sort((a, b) => Date.parse(b.requestedAt) - Date.parse(a.requestedAt));
  data.returned = data.operations.length;
  add('history.json', entry.meta, data);
}

// -- status -----------------------------------------------------------------

add('status.full.json', {
  command: 'status',
  variant: 'full',
  schema: 'status.schema.json',
  description: 'A complete snapshot inside its budget: three services, one running operation, one queued, five finished, a policy with a nightly window, and every 2.x key still mirroring the first service.',
  tags: ['baseline', '2x-compatibility'],
}, status({
  operations: {
    running: [summarise(OP_UPDATE_RUNNING)],
    queued: [summarise(OP_RESTART_QUEUED)],
    recent: [
      summarise(OP_RUN),
      summarise(OP_POSTCONDITION),
      summarise(OP_UPDATE_DONE),
      summarise(OP_CYCLE),
      summarise(OP_ROLLED_BACK),
    ],
  },
  notes: [
    'edge: no busy probe is configured, so disruptive work on this machine waits. Set busy.type to "none" if edge is never busy.',
    'demo: automatic updates are off for this service; a manual update still runs.',
  ],
}));

add('status.partial.json', {
  command: 'status',
  variant: 'partial',
  schema: 'status.schema.json',
  description: 'The budget ran out with a probe still in flight. Every row that is there is true; the one that is not says so with evidence "timed-out", and the answer arrives on time instead of arriving right.',
  tags: ['bounded-status', 'restricted-session'],
}, status({
  timing: { budgetMs: 20000, elapsedMs: 20000, partial: true },
  agent: {
    version: AGENT,
    contract: 3,
    base: '/home/x1f4r/.legion-control',
    node: '24.9.0',
    restrictedSession: true,
  },
  services: [
    { ...SERVICE_T3, busy: BUSY_TIMED_OUT, updates: serviceUpdates({ eligibleNow: false, deferredReason: 'busy-unknown' }) },
    { ...SERVICE_DEMO, health: { ok: false, status: 0, checkedAt: null, elapsedMs: null, error: 'the health check did not run: the status budget was spent' } },
    SERVICE_EDGE,
  ],
  operations: { running: [summarise(OP_UPDATE_RUNNING)], queued: [], recent: [] },
  notes: [
    't3 busy: the busy probe did not answer within 8000 ms, so it counts as busy.',
    'This snapshot is partial: 2 of 6 probes did not finish inside the 20000 ms budget.',
  ],
}));

add('status.config-invalid.json', {
  command: 'status',
  variant: 'config-invalid',
  schema: 'status.schema.json',
  description: 'The configuration file on the machine will not parse, so the agent is answering from the last copy that did. Reading still works and every mutation is refused with config-invalid until somebody fixes the file.',
  tags: ['config', 'last-known-good'],
}, status({
  config: {
    ok: false,
    source: 'last-known-good',
    problems: [
      {
        level: 'error',
        path: 'config.json',
        message: 'the file does not parse: Unexpected token } in JSON at position 812',
        fix: 'fix the trailing comma at line 34, or delete config.json to start from defaults',
      },
      {
        level: 'warning',
        path: 'services[2].busy',
        message: 'edge has no busy probe, so nothing on this machine can be stopped without a force',
        fix: 'add a busy probe for edge, or set busy.type to "none" if it is never busy',
      },
    ],
  },
  services: [SERVICE_T3, SERVICE_DEMO, SERVICE_EDGE],
  operations: { running: [], queued: [], recent: [summarise(OP_UPDATE_DONE)] },
  notes: [
    'config.json does not parse; this snapshot comes from config.last-good.json, saved 2026-09-03T14:00:00.000Z.',
    'Every update, restart, boot, sleep and run is refused until the file parses.',
  ],
}));

add('status.no-services.json', {
  command: 'status',
  variant: 'no-services',
  schema: 'status.schema.json',
  description: 'A freshly installed agent with no configuration at all: inert defaults, no services, automatic updates off, and nothing to be busy about. The shape a client sees before anybody has set the machine up.',
  tags: ['defaults', 'empty'],
}, status({
  hostname: 'newpi',
  services: [],
  config: { ok: true, source: 'defaults', problems: [] },
  automatic: false,
  maintenanceWindows: [],
  inWindowNow: true,
  nextWindow: null,
  lastCycle: null,
  bootTargets: [],
  actions: [],
  controller: { hash: null, id: null, revision: 0, updatedAt: null, source: null },
  timing: { budgetMs: 20000, elapsedMs: 42, partial: false },
  notes: [
    'No configuration file was found, so this agent is running on inert defaults: no services, and automatic updates off.',
    'Write /home/x1f4r/.legion-control/config.json to describe what this machine runs.',
  ],
}));

// -- busy -------------------------------------------------------------------

add('busy.single-service.json', {
  command: 'busy',
  variant: 'single-service',
  schema: 'busy.schema.json',
  description: 'A machine with exactly one service answers in the 2.x shape: that service’s own busy object spread across the top level. A 2.x client reads it unchanged.',
  tags: ['2x-compatibility'],
}, {
  ...envelope(),
  ...BUSY_T3,
  monitoredServices: 1,
  unmonitoredServices: 0,
});

add('busy.several-services.json', {
  command: 'busy',
  variant: 'several-services',
  schema: 'busy.schema.json',
  description: 'Three services, three different kinds of evidence, and one aggregate. The aggregate is busy because a service is busy and unknown because a probe could not be read; both have to be true for the gate to fail closed.',
  tags: ['aggregate'],
}, {
  ...envelope(),
  busy: true,
  unknown: true,
  reason: 'T3 Code: 1 turn running; Edge Proxy: not monitored',
  monitoredServices: 2,
  unmonitoredServices: 1,
  services: [
    { id: 't3', name: 'T3 Code', ...BUSY_T3 },
    { id: 'demo', name: 'Demo', ...BUSY_DEMO_IDLE },
    { id: 'edge', name: 'Edge Proxy', ...BUSY_UNMONITORED },
  ],
});

add('busy.declared-none.json', {
  command: 'busy',
  variant: 'declared-none',
  schema: 'busy.schema.json',
  description: 'A service whose config says busy.type is "none". That is an answer, not the absence of one, so it counts as monitored and nothing warns about it. The service beside it, which nobody described at all, is the one that blocks.',
  tags: ['busy-gate', 'fail-closed'],
}, {
  ...envelope(),
  busy: true,
  unknown: true,
  reason: 'Edge Proxy: not monitored',
  monitoredServices: 1,
  unmonitoredServices: 1,
  services: [
    { id: 'demo', name: 'Demo', ...BUSY_DECLARED_NONE },
    { id: 'edge', name: 'Edge Proxy', ...BUSY_UNMONITORED },
  ],
});

add('busy.idle.json', {
  command: 'busy',
  variant: 'idle',
  schema: 'busy.schema.json',
  description: 'Nothing running, and one stale pending turn that is not blocking. A row that has been pending for three days is not work in progress, and ageing it out is only allowed for pending rows.',
  tags: ['aggregate'],
}, {
  ...envelope(),
  busy: false,
  unknown: false,
  reason: 'idle',
  monitoredServices: 2,
  unmonitoredServices: 0,
  services: [
    { id: 't3', name: 'T3 Code', ...BUSY_T3_IDLE },
    { id: 'demo', name: 'Demo', ...BUSY_DEMO_IDLE },
  ],
});

// -- update -----------------------------------------------------------------

add('update.accepted.json', {
  command: 'update',
  variant: 'accepted',
  schema: 'update.schema.json',
  description: 'The immediate answer to --detach. The work is running under the id the client supplied; the client now polls op ID --wait 20. This is not a claim that anything finished.',
  tags: ['detach', 'operations'],
}, {
  ...envelope({ message: 'update of t3 accepted; poll operation 6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3' }),
  action: 'accepted',
  service: 't3',
  from: '0.0.36-nightly.20260904',
  to: '0.0.37-nightly.20260905',
  detached: true,
  op: OP_UPDATE_RUNNING,
});

add('update.updated.json', {
  command: 'update',
  variant: 'updated',
  schema: 'update.schema.json',
  description: 'A synchronous update that finished. `from` and `to` are the versions read on the machine after the install, never the version that was aimed at.',
  tags: ['happy-path'],
}, {
  ...envelope({ message: 'updated t3 from 0.0.35-nightly.20260903 to 0.0.36-nightly.20260904' }),
  action: 'updated',
  service: 't3',
  from: '0.0.35-nightly.20260903',
  to: '0.0.36-nightly.20260904',
  detached: false,
  op: OP_UPDATE_DONE,
});

add('update.noop.json', {
  command: 'update',
  variant: 'noop',
  schema: 'update.schema.json',
  description: 'Nothing to install. The one benign "nothing happened", and the reason it needs a code: the other nine look identical without one.',
  tags: ['reason-codes'],
}, {
  ...envelope({ reasonCode: 'no-update', message: 'demo is already on 1.4.2' }),
  action: 'noop',
  service: 'demo',
  from: '1.4.2',
  to: '1.4.2',
  op: null,
});

add('update.deferred-busy.json', {
  command: 'update',
  variant: 'deferred-busy',
  schema: 'update.schema.json',
  description: 'The busy gate held. Deferring costs a delay and the next cycle tries again; going ahead costs somebody their work, which is why this is the default and --force is a decision a person makes.',
  tags: ['busy-gate'],
}, {
  ...envelope({ reasonCode: 'busy', message: 't3 is busy (1 turn running); pass --force to update anyway' }),
  action: 'deferred',
  service: 't3',
  from: '0.0.36-nightly.20260904',
  to: '0.0.37-nightly.20260905',
  op: null,
});

add('update.queued.json', {
  command: 'update',
  variant: 'queued',
  schema: 'update.schema.json',
  description: '--when-idle. The request is held with an expiry and runs on the next cycle that finds the machine idle. A second when-idle request for the same service replaces this one, and the reply names the id it displaced.',
  tags: ['queue'],
}, {
  ...envelope({ message: 't3 will be updated on the next idle cycle, or dropped at 2026-09-05T18:00:00.000Z' }),
  action: 'queued',
  service: 't3',
  from: '0.0.36-nightly.20260904',
  to: '0.0.37-nightly.20260905',
  replaced: OPS.expired,
  op: record({
    id: '5f0b2e97-3c48-4d16-b8a2-1e7d9f04c635',
    kind: 'update',
    service: 't3',
    mode: 'queued',
    initiator: INITIATOR.phone,
    state: 'queued',
    phase: 'queued',
    requestedAt: T(0),
    startedAt: null,
    updatedAt: T(0),
    expiresAt: T(240),
    pid: null,
    detached: false,
    from: '0.0.36-nightly.20260904',
    to: '0.0.37-nightly.20260905',
    replaced: OPS.expired,
    log: [{ at: T(0), line: 'queued until idle, replacing 2b7e4d18-9a63-4f52-8c07-3e1d6a45b980' }],
  }),
});

add('update.conflict.json', {
  command: 'update',
  variant: 'conflict',
  schema: 'update.schema.json',
  description: 'Another operation holds the machine-wide lock. The reply names it, so the app can say what is in the way instead of saying "busy". --force never breaks this lock.',
  tags: ['operation-lock'],
}, {
  ...envelope({
    reasonCode: 'operation-in-progress',
    message: 'an update of t3 has held the lock since 2026-09-05T14:00:01.000Z; this request was not started',
  }),
  action: 'conflict',
  service: 'demo',
  from: null,
  to: null,
  op: null,
  conflict: {
    opId: OPS.updateRunning,
    kind: 'update',
    service: 't3',
    phase: 'installing',
    startedAt: T(0, 1),
  },
});

add('update.replayed.json', {
  command: 'update',
  variant: 'replayed-running',
  schema: 'update.schema.json',
  description: 'The same --op id sent twice because the first reply was lost. The work is not started again; the existing record comes back. This is what makes a mutation safe to retry over a link that drops.',
  tags: ['idempotency'],
}, {
  ...envelope({
    reasonCode: 'already-running',
    message: 'operation 6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3 is already running; nothing was started twice',
  }),
  action: 'already-running',
  service: 't3',
  from: '0.0.36-nightly.20260904',
  to: '0.0.37-nightly.20260905',
  replayed: true,
  detached: true,
  op: OP_UPDATE_RUNNING,
});

add('update.replayed-finished.json', {
  command: 'update',
  variant: 'replayed-finished',
  schema: 'update.schema.json',
  description: 'The same id again, after the work finished. The finished result comes back with replayed true, so a client that retried gets the answer it lost rather than a second install.',
  tags: ['idempotency'],
}, {
  ...envelope({ message: 'updated t3 from 0.0.35-nightly.20260903 to 0.0.36-nightly.20260904' }),
  action: 'updated',
  service: 't3',
  from: '0.0.35-nightly.20260903',
  to: '0.0.36-nightly.20260904',
  replayed: true,
  op: OP_UPDATE_DONE,
});

add('update.rolled-back.json', {
  command: 'update',
  variant: 'rolled-back',
  schema: 'update.schema.json',
  description: 'The new version would not come up, so the old one was put back. The service is running; the update is not ok. Both halves have to be said, and `to` is the version actually installed at the end.',
  tags: ['recovery'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'rolled-back',
    message: 'demo 1.5.0 would not answer its health check, so 1.4.2 was put back and is running',
  }),
  action: 'rolled-back',
  service: 'demo',
  from: '1.4.2',
  to: '1.4.2',
  op: OP_ROLLED_BACK,
});

add('update.postcondition-failed.json', {
  command: 'update',
  variant: 'postcondition-failed',
  schema: 'update.schema.json',
  description: 'The update command exited 0 and changed nothing. An exit code is not a postcondition: what counts is the version read back afterwards, and `to` reports that rather than what was aimed at.',
  tags: ['postcondition'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'postcondition-failed',
    message: 'the update command exited 0 and demo is still on 1.4.2, not the 1.5.1 that was expected',
  }),
  action: 'failed',
  service: 'demo',
  from: '1.4.2',
  to: '1.4.2',
  verified: false,
  op: OP_POSTCONDITION,
});

add('update.interrupted.json', {
  command: 'update',
  variant: 'interrupted',
  schema: 'update.schema.json',
  description: 'A detached worker died mid-install and the recovery pass finished the record. The service was started again, the version is the one it had, and nothing here is dressed up as success.',
  tags: ['recovery'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'interrupted',
    message: 'the worker holding this operation is gone and the installed version is 0.0.31-nightly.20260901, so the install did not complete; t3 was started again and left on the version it had',
  }),
  action: 'interrupted',
  service: 't3',
  from: '0.0.31-nightly.20260901',
  to: '0.0.31-nightly.20260901',
  op: OP_INTERRUPTED,
});

add('update.latest-unknown.json', {
  command: 'update',
  variant: 'latest-unknown',
  schema: 'update.schema.json',
  description: 'The registry could not be reached, so there is nothing to compare against. Not up to date, not out of date: unknown. --force does not turn this into a success, because there is no version to install.',
  tags: ['reason-codes', 'honesty'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'latest-unknown',
    message: 'the newest version of t3 could not be looked up (npm registry timed out after 10000 ms), so there is nothing to compare 0.0.36-nightly.20260904 against',
  }),
  action: 'failed',
  service: 't3',
  from: '0.0.36-nightly.20260904',
  to: '0.0.36-nightly.20260904',
  op: null,
});

add('update.not-installed.json', {
  command: 'update',
  variant: 'not-installed',
  schema: 'update.schema.json',
  description: 'Nothing to update, because nothing is installed. Separate from no-update on purpose: one means "you have the newest", the other means "you have none".',
  tags: ['reason-codes'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'not-installed',
    message: 'demo is not installed on this machine, so there was nothing to update',
  }),
  action: 'failed',
  service: 'demo',
  from: null,
  to: null,
  op: null,
});

add('update.forced-unverified.json', {
  command: 'update',
  variant: 'forced-unverified',
  schema: 'update.schema.json',
  description: 'A command service with no latestVersion and no verify command, updated under --force. The command ran and exited 0, and the reply says outright that nothing was verified: verified false is the honest half of this answer.',
  tags: ['postcondition', 'force'],
}, {
  ...envelope({
    message: 'the update command for demo exited 0; there is no version to read back and no verify command, so nothing was checked',
  }),
  action: 'updated',
  service: 'demo',
  from: null,
  to: null,
  verified: false,
  op: record({
    id: '7a4c1b58-3e90-4d27-8f61-2c9b5a03e746',
    kind: 'update',
    service: 'demo',
    mode: 'force',
    initiator: INITIATOR.desktop,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-15),
    startedAt: T(-15),
    updatedAt: T(-14),
    finishedAt: T(-14),
    pid: null,
    detached: false,
    from: null,
    to: null,
    result: {
      ok: true,
      action: 'updated',
      reasonCode: null,
      message: 'the update command exited 0; nothing was verified',
      from: null,
      to: null,
      exitCode: 0,
      output: 'pulled 3 objects\n',
      verified: false,
    },
    log: [{ at: T(-15), line: 'no latestVersion and no verify command configured; --force given' }],
  }),
});

add('update.app-closed.json', {
  command: 'update',
  variant: 'app-closed',
  schema: 'update.schema.json',
  description: 'A macOS app service whose updater only applies a staged build on quit. The build is downloaded and waiting; the app has to be closed for it to land, and saying so is more use than "failed".',
  tags: ['mac', 'reason-codes'],
}, {
  ...envelope({
    system: MAC,
    reasonCode: 'app-closed',
    message: 'T3 Desktop 1.9.4 is downloaded and applies when the app quits; it is still running, so nothing was replaced',
  }),
  action: 'deferred',
  service: 't3-desktop',
  from: '1.9.3',
  to: '1.9.3',
  op: null,
});

add('update.apply-failed.json', {
  command: 'update',
  variant: 'apply-failed',
  schema: 'update.schema.json',
  description: 'The staged build was there and would not apply. The app is left on the version it had, which is the only outcome that keeps the machine usable.',
  tags: ['mac', 'reason-codes'],
}, {
  ...envelope({
    system: MAC,
    ok: false,
    reasonCode: 'apply-failed',
    message: 'the staged T3 Desktop 1.9.4 could not be applied (the pending bundle is not signed by the expected team); 1.9.3 is still installed and running',
  }),
  action: 'failed',
  service: 't3-desktop',
  from: '1.9.3',
  to: '1.9.3',
  op: record({
    id: OPS.applyFailed,
    kind: 'update',
    service: 't3-desktop',
    mode: 'manual',
    initiator: INITIATOR.mac,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-45),
    startedAt: T(-45),
    updatedAt: T(-44),
    finishedAt: T(-44),
    pid: null,
    detached: false,
    systemId: 'mac',
    from: '1.9.3',
    to: '1.9.3',
    result: {
      ok: false,
      action: 'failed',
      reasonCode: 'apply-failed',
      message: 'the pending bundle is not signed by the expected team',
      from: '1.9.3',
      to: '1.9.3',
      exitCode: 1,
      output: null,
    },
    log: [{ at: T(-45), line: 'staged 1.9.4 found in the pending directory' }],
  }),
});

// -- restart ----------------------------------------------------------------

add('restart.restarted.json', {
  command: 'restart',
  variant: 'restarted',
  schema: 'restart.schema.json',
  description: 'A restart that came back healthy. The health check after the start is what turns "the command exited 0" into "the service is answering".',
  tags: ['happy-path'],
}, {
  ...envelope({ message: 't3 restarted and answered on 8081' }),
  action: 'restarted',
  service: 't3',
  detached: false,
  op: record({
    id: 'c92e6b41-5a08-4f37-b6d9-0e34c7a18ف25'.replace(/[^a-z0-9-]/g, '') + 'd3',
    kind: 'restart',
    service: 't3',
    mode: 'manual',
    initiator: INITIATOR.mac,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-10),
    startedAt: T(-10),
    updatedAt: T(-9),
    finishedAt: T(-9),
    pid: null,
    detached: false,
    result: {
      ok: true,
      action: 'restarted',
      reasonCode: null,
      message: 't3 restarted and answered on 8081',
      exitCode: 0,
      output: null,
    },
    log: [
      { at: T(-10), line: 'lock taken for restart t3' },
      { at: T(-10, 2), line: 'stopped t3-code.service' },
      { at: T(-9), line: 'started t3-code.service, health 200' },
    ],
  }),
});

add('restart.deferred-busy-unknown.json', {
  command: 'restart',
  variant: 'deferred-busy-unknown',
  schema: 'restart.schema.json',
  description: 'The probe could not be read, so the answer is not "idle". busy-unknown is a separate code from busy because the fix is different: one is "wait", the other is "somebody has to look at the machine".',
  tags: ['busy-gate', 'fail-closed'],
}, {
  ...envelope({
    reasonCode: 'busy-unknown',
    message: 'whether edge is busy could not be determined (no busy probe is configured), so the restart was not run; pass --force to restart anyway',
  }),
  action: 'deferred',
  service: 'edge',
  op: null,
});

add('restart.queued.json', {
  command: 'restart',
  variant: 'queued',
  schema: 'restart.schema.json',
  description: 'A restart held until the machine is idle, with the expiry that stops the queue from filling up with requests nobody remembers making.',
  tags: ['queue'],
}, {
  ...envelope({ message: 't3 will be restarted on the next idle cycle, or dropped at 2026-09-05T17:57:00.000Z' }),
  action: 'queued',
  service: 't3',
  replaced: null,
  op: OP_RESTART_QUEUED,
});

// -- boot -------------------------------------------------------------------

add('boot.rebooting.json', {
  command: 'boot',
  variant: 'rebooting',
  schema: 'boot.schema.json',
  description: 'Armed, read back from the firmware, and rebooting. The read-back is the whole point: a reboot on an unarmed machine comes back on the same system, and reporting that as success sends the controller off waiting for a machine that never appears.',
  tags: ['boot', 'read-back'],
}, {
  ...envelope({ message: 'BootNext set to 0002 (Windows 11) and read back; reboot scheduled in 3 seconds' }),
  action: 'rebooting',
  target: 'windows',
  armed: true,
  op: record({
    id: OPS.boot,
    kind: 'boot',
    target: 'windows',
    mode: 'manual',
    initiator: INITIATOR.phone,
    state: 'running',
    phase: 'rebooting',
    requestedAt: T(-2880),
    startedAt: T(-2880),
    updatedAt: T(-2880, 3),
    pid: 20114,
    detached: false,
    log: [
      { at: T(-2880), line: 'BootNext set to 0002 and read back' },
      { at: T(-2880, 3), line: 'reboot scheduled' },
    ],
  }),
});

add('boot.armed.json', {
  command: 'boot',
  variant: 'armed',
  schema: 'boot.schema.json',
  description: '--no-reboot: the next boot is pointed at Windows and the machine keeps running. Useful on its own, and the half of boot that can be verified without losing the session.',
  tags: ['boot'],
}, {
  ...envelope({ message: 'BootNext set to 0002 (Windows 11) and read back; the machine was not rebooted' }),
  action: 'armed',
  target: 'windows',
  armed: true,
  op: record({
    id: '3f8b0d27-6c14-4a95-b7e2-8d05f1a3c64b',
    kind: 'boot',
    target: 'windows',
    mode: 'manual',
    initiator: INITIATOR.desktop,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-500),
    startedAt: T(-500),
    updatedAt: T(-500, 1),
    finishedAt: T(-500, 1),
    pid: null,
    detached: false,
    result: {
      ok: true,
      action: 'armed',
      reasonCode: null,
      message: 'BootNext set to 0002 and read back',
      exitCode: 0,
      output: null,
      verified: true,
    },
    log: [{ at: T(-500), line: 'BootNext set to 0002 and read back' }],
  }),
});

add('boot.already-on-target.json', {
  command: 'boot',
  variant: 'already-on-target',
  schema: 'boot.schema.json',
  description: 'Asked to boot into the system it is already running. Nothing to do, and nothing pretending to have been done.',
  tags: ['reason-codes'],
}, {
  ...envelope({
    system: WINDOWS,
    reasonCode: 'already-on-target',
    message: 'this machine is already running Windows 11',
  }),
  action: 'noop',
  target: 'windows',
  op: null,
});

add('boot.queued.json', {
  command: 'boot',
  variant: 'queued',
  schema: 'boot.schema.json',
  description: 'Boot into Windows once the machine is idle. The cycle arms and reboots it exactly as a manual request would; nothing about being queued makes the transition softer.',
  tags: ['queue', 'boot'],
}, {
  ...envelope({ message: 'the machine will boot into Windows 11 on the next idle cycle, or the request is dropped at 2026-09-05T14:40:00.000Z' }),
  action: 'queued',
  target: 'windows',
  replaced: null,
  op: {
    ...OP_CANCELLED,
    state: 'queued',
    phase: 'queued',
    updatedAt: T(-200),
    finishedAt: null,
    result: null,
    log: [{ at: T(-200), line: 'queued until idle, expires in 4h' }],
  },
});

// -- sleep ------------------------------------------------------------------

add('sleep.sleeping.json', {
  command: 'sleep',
  variant: 'sleeping',
  schema: 'sleep.schema.json',
  description: 'The suspend was accepted. That is all a process about to be frozen can honestly report: `slept` is only ever written later, by the recovery pass, once the machine has come back.',
  tags: ['honesty'],
}, {
  ...envelope({ message: 'systemctl suspend was accepted; whether the machine actually suspended is not something this process can see' }),
  action: 'sleeping',
  op: OP_SLEEP,
});

add('sleep.queued.json', {
  command: 'sleep',
  variant: 'queued',
  schema: 'sleep.schema.json',
  description: 'Sleep when idle. The expiry matters more here than anywhere: a queued sleep that fires two days late suspends a machine somebody is sitting at.',
  tags: ['queue'],
}, {
  ...envelope({ message: 'the machine will suspend on the next idle cycle, or the request is dropped at 2026-09-05T16:00:00.000Z' }),
  action: 'queued',
  replaced: null,
  op: record({
    id: 'ae61c937-08d5-4b2f-9137-4c6e0a85b2d7',
    kind: 'sleep',
    mode: 'queued',
    initiator: INITIATOR.phone,
    state: 'queued',
    phase: 'queued',
    requestedAt: T(-2),
    startedAt: null,
    updatedAt: T(-2),
    expiresAt: T(118),
    pid: null,
    detached: false,
    log: [{ at: T(-2), line: 'queued until idle, expires in 2h' }],
  }),
});

// -- run --------------------------------------------------------------------

add('run.ran.json', {
  command: 'run',
  variant: 'ran',
  schema: 'run.schema.json',
  description: 'A configured action that ran. `output` is what the command printed; the agent has always returned it and the apps now show it, which is the difference between "the action ran" and knowing what it did.',
  tags: ['happy-path'],
}, {
  ...envelope({ message: 'sunshine-restart finished with exit code 0' }),
  action: 'ran',
  id: 'sunshine-restart',
  exitCode: 0,
  output: 'Stopping sunshine.service\nStarting sunshine.service\nsunshine.service is active',
  detached: false,
  op: OP_RUN,
});

add('run.failed.json', {
  command: 'run',
  variant: 'failed',
  schema: 'run.schema.json',
  description: 'The action ran and exited non-zero. The output is the diagnosis, so it is returned rather than summarised away.',
  tags: ['failure'],
}, {
  ...envelope({ ok: false, reasonCode: 'not-running', message: 'flush-cache exited 3' }),
  action: 'failed',
  id: 'flush-cache',
  exitCode: 3,
  output: 'error: the build daemon is not running, so there is no cache to flush\n',
  op: record({
    id: 'bd407e15-9c62-4a38-85f1-3d0b7e2a6c94',
    kind: 'run',
    actionId: 'flush-cache',
    mode: 'manual',
    initiator: INITIATOR.desktop,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-20),
    startedAt: T(-20),
    updatedAt: T(-20, 1),
    finishedAt: T(-20, 1),
    pid: null,
    detached: false,
    result: {
      ok: false,
      action: 'failed',
      reasonCode: 'not-running',
      message: 'flush-cache exited 3',
      exitCode: 3,
      output: 'error: the build daemon is not running, so there is no cache to flush\n',
    },
    log: [{ at: T(-20), line: 'run flush-cache' }],
  }),
});

add('run.wol-ran.json', {
  command: 'run',
  variant: 'wol-ran',
  schema: 'run.schema.json',
  description: 'A wake action on a helper. The agent sends the magic packets itself, so a Pi on the target LAN is a wake helper without a wakeonlan binary anywhere. Note what the output does NOT claim: packets were sent, not that anything woke up. Readiness is the authenticated poll that follows, because a UDP datagram is never evidence of a machine coming back.',
  tags: ['wake', 'helper', 'honesty'],
}, {
  ...envelope({ message: 'sent 6 packets to 192.168.178.255:9, 192.168.178.255:7' }),
  action: 'ran',
  id: 'wake-legion',
  exitCode: 0,
  output: 'sent 6 packets to 192.168.178.255:9, 192.168.178.255:7',
  detached: false,
  op: record({
    id: 'f28a4c60-7b13-4e95-8d02-6a1c39f7b504',
    kind: 'run',
    actionId: 'wake-legion',
    mode: 'manual',
    initiator: INITIATOR.phone,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-4),
    startedAt: T(-4),
    updatedAt: T(-4, 0, 240),
    finishedAt: T(-4, 0, 240),
    pid: null,
    detached: false,
    result: {
      ok: true,
      action: 'ran',
      reasonCode: null,
      message: 'sent 6 packets to 192.168.178.255:9, 192.168.178.255:7',
      exitCode: 0,
      output: 'sent 6 packets to 192.168.178.255:9, 192.168.178.255:7',
    },
    log: [{ at: T(-4), line: 'wol wake-legion: 3 repeats to 2 addresses for AA:BB:CC:DD:EE:FF' }],
  }),
});

add('run.wol-failed.json', {
  command: 'run',
  variant: 'wol-failed',
  schema: 'run.schema.json',
  description: 'Not one packet left the helper. Named addresses and a real failure, so the client can move to the next helper in the list instead of waiting for a machine that was never called. A wol action is a pure declared send, so retrying it on another helper is safe; a general command action is not, and has to be reconciled by its operation id first.',
  tags: ['wake', 'helper', 'failure'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'internal',
    message: 'no packets could be sent: 192.168.178.255:9 and 192.168.178.255:7 both failed with EACCES (the broadcast socket was refused)',
  }),
  action: 'failed',
  id: 'wake-legion',
  exitCode: 1,
  output: null,
  op: record({
    id: '0b93e5a7-4c18-4d62-9ف08-3e71a5c40b26'.replace(/[^a-z0-9-]/g, '') + 'f4',
    kind: 'run',
    actionId: 'wake-legion',
    mode: 'manual',
    initiator: INITIATOR.desktop,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-5),
    startedAt: T(-5),
    updatedAt: T(-5, 0, 90),
    finishedAt: T(-5, 0, 90),
    pid: null,
    detached: false,
    result: {
      ok: false,
      action: 'failed',
      reasonCode: 'internal',
      message: 'no packets could be sent: 192.168.178.255:9 and 192.168.178.255:7 both failed with EACCES',
      exitCode: 1,
      output: null,
    },
    log: [{ at: T(-5), line: 'wol wake-legion: EACCES on 192.168.178.255:9' }],
  }),
});

add('run.timed-out.json', {
  command: 'run',
  variant: 'timed-out',
  schema: 'run.schema.json',
  description: 'The action overran its configured timeout and was killed. Timed out is not failed: whether it did anything before it was killed is unknown, and the reply says so rather than guessing.',
  tags: ['failure', 'honesty'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'timed-out',
    message: 'flush-cache did not finish within 120 s and was killed; whether it changed anything before that is not known',
  }),
  action: 'failed',
  id: 'flush-cache',
  exitCode: null,
  output: 'clearing 41 GB\n',
  op: record({
    id: '6e0d95c4-2b71-4f38-a9d6-15c8e30b7f42',
    kind: 'run',
    actionId: 'flush-cache',
    mode: 'manual',
    initiator: INITIATOR.phone,
    state: 'finished',
    phase: 'done',
    requestedAt: T(-8),
    startedAt: T(-8),
    updatedAt: T(-6),
    finishedAt: T(-6),
    pid: null,
    detached: false,
    result: {
      ok: false,
      action: 'failed',
      reasonCode: 'timed-out',
      message: 'flush-cache did not finish within 120 s and was killed',
      exitCode: null,
      output: 'clearing 41 GB\n',
    },
    log: [{ at: T(-8), line: 'run flush-cache, timeout 120 s' }],
  }),
});

// -- cycle ------------------------------------------------------------------

add('cycle.cycled.json', {
  command: 'cycle',
  variant: 'cycled',
  schema: 'cycle.schema.json',
  description: 'The scheduled pass. One service updated and two were skipped for different reasons; one deferral never stops the rest, which is the whole point of walking them under one parent record.',
  tags: ['scheduler'],
}, {
  ...envelope({ message: '3 services walked: 1 updated, 2 deferred' }),
  action: 'cycled',
  dryRun: false,
  op: OP_CYCLE,
  children: OP_CYCLE.children,
  queued: [],
  recovered: [],
});

add('cycle.dry-run.json', {
  command: 'cycle',
  variant: 'dry-run',
  schema: 'cycle.schema.json',
  description: 'What a cycle would do right now, without taking the lock or touching anything. The five ways a service can be ineligible, side by side: this is the fixture the policy screens are drawn from.',
  tags: ['scheduler', 'policy', 'reason-codes'],
}, {
  ...envelope({ message: 'nothing would run: 5 services, none eligible' }),
  action: 'skipped',
  dryRun: true,
  op: null,
  plan: [
    { service: 't3', wouldRun: false, reasonCode: 'busy', detail: '1 turn running' },
    { service: 'demo', wouldRun: false, reasonCode: 'policy-off', detail: 'automatic updates are off for demo' },
    { service: 'edge', wouldRun: false, reasonCode: 'busy-unknown', detail: 'no busy probe is configured for edge' },
    { service: 'api', wouldRun: false, reasonCode: 'outside-window', detail: 'the maintenance window is 02:00 to 06:00 and it is 14:00' },
    { service: 'web', wouldRun: false, reasonCode: 'policy-paused', detail: 'updates are paused for web until 2026-09-08T02:00:00.000Z' },
  ],
});

add('cycle.conflict.json', {
  command: 'cycle',
  variant: 'conflict',
  schema: 'cycle.schema.json',
  description: 'The scheduler fired while a manual operation held the lock. The cycle steps aside and says who has it; the next scheduled run picks the work up.',
  tags: ['scheduler', 'operation-lock'],
}, {
  ...envelope({
    reasonCode: 'lock-held',
    message: 'an update of t3 has held the lock since 2026-09-05T14:00:01.000Z; this cycle did nothing',
  }),
  action: 'conflict',
  dryRun: false,
  op: null,
  children: [],
  conflict: {
    opId: OPS.updateRunning,
    kind: 'update',
    service: 't3',
    phase: 'installing',
    startedAt: T(0, 1),
  },
});

// -- op / cancel ------------------------------------------------------------

add('op.running.json', {
  command: 'op',
  variant: 'running',
  schema: 'op.schema.json',
  description: 'A record read back while it is still running, with the phase and the progress note the app draws under the machine. The wait hit its deadline, which is not a failure.',
  tags: ['operations', 'polling'],
}, {
  ...envelope(),
  id: OPS.updateRunning,
  op: OP_UPDATE_RUNNING,
  waited: { requestedSeconds: 20, elapsedMs: 20004, finished: false },
});

add('op.queued.json', {
  command: 'op',
  variant: 'queued',
  schema: 'op.schema.json',
  description: 'A queued record. It has an expiry and no pid, because nothing is running it yet.',
  tags: ['operations', 'queue'],
}, {
  ...envelope(),
  id: OPS.restartQueued,
  op: OP_RESTART_QUEUED,
});

add('op.finished-updated.json', {
  command: 'op',
  variant: 'finished-updated',
  schema: 'op.schema.json',
  description: 'A finished record with its log. This is what a client that lost its link comes back to, and it is why a lost reply no longer means an unknown machine.',
  tags: ['operations'],
}, {
  ...envelope(),
  id: OPS.updateDone,
  op: OP_UPDATE_DONE,
  waited: { requestedSeconds: 20, elapsedMs: 62, finished: true },
});

add('op.finished-rebooted.json', {
  command: 'op',
  variant: 'finished-rebooted',
  schema: 'op.schema.json',
  description: 'A boot resolved after the machine came back. The evidence is named in the message: the running system is the target and the uptime is shorter than the record, so the transition happened. A disconnection alone would never have been enough.',
  tags: ['operations', 'boot', 'evidence'],
}, {
  ...envelope(),
  id: OPS.boot,
  op: OP_BOOT_REBOOTED,
});

add('op.finished-interrupted.json', {
  command: 'op',
  variant: 'finished-interrupted',
  schema: 'op.schema.json',
  description: 'The record the recovery pass closed when it found the worker gone. Interrupted, not failed and not succeeded: the outcome is known to be incomplete, and the machine was left in a state that runs.',
  tags: ['operations', 'recovery'],
}, {
  ...envelope(),
  id: OPS.interrupted,
  op: OP_INTERRUPTED,
});

add('op.finished-rolled-back.json', {
  command: 'op',
  variant: 'finished-rolled-back',
  schema: 'op.schema.json',
  description: 'A rollback, from the inside. The log is the evidence that the old version is the one running now.',
  tags: ['operations', 'recovery'],
}, {
  ...envelope(),
  id: OPS.rolledBack,
  op: OP_ROLLED_BACK,
});

add('op.not-found.json', {
  command: 'op',
  variant: 'not-found',
  schema: 'op.schema.json',
  description: 'An id this machine has no record of. Distinct from a record that exists and failed: a client can tell "the request never arrived" from "the request arrived and went wrong", and only the first is safe to send again.',
  tags: ['operations', 'idempotency'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'not-configured',
    message: 'no operation 11111111-2222-4333-8444-555555555555 on this machine; it either never arrived or has been pruned',
  }),
  id: '11111111-2222-4333-8444-555555555555',
  op: null,
});

add('cancel.cancelled.json', {
  command: 'cancel',
  variant: 'cancelled',
  schema: 'cancel.schema.json',
  description: 'A queued operation dropped before it ran.',
  tags: ['queue'],
}, {
  ...envelope({ message: 'the queued boot into windows was cancelled before it ran' }),
  id: OPS.cancelled,
  action: 'cancelled',
  op: OP_CANCELLED,
});

add('cancel.conflict-running.json', {
  command: 'cancel',
  variant: 'conflict-running',
  schema: 'cancel.schema.json',
  description: 'A running operation is not killable. There is no safe way to interrupt work that has already stopped a service, so cancel refuses and says what is running.',
  tags: ['operation-lock'],
}, {
  ...envelope({
    reasonCode: 'already-running',
    message: 'operation 6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3 is already running and cannot be cancelled; wait for it to finish',
  }),
  id: OPS.updateRunning,
  action: 'conflict',
  op: OP_UPDATE_RUNNING,
});

// -- history / logs ---------------------------------------------------------

addHistory({
  meta: {
    command: 'history',
    variant: 'default',
    schema: 'history.schema.json',
    description: 'The last operations, newest first, as the machine section draws them. Seven different outcomes, including the two nobody plans for: an update that ran out of apply attempts, and a queued request that expired unrun.',
    tags: ['operations', 'reason-codes'],
  },
  data: {
  ...envelope(),
  operations: [
    summarise(OP_UPDATE_RUNNING),
    summarise(OP_RESTART_QUEUED),
    summarise(OP_RUN),
    summarise(OP_POSTCONDITION),
    {
      id: OPS.expired,
      kind: 'update',
      service: 't3',
      target: null,
      actionId: null,
      mode: 'queued',
      state: 'finished',
      phase: 'done',
      action: 'expired',
      reasonCode: 'expired',
      requestedAt: T(-300),
      updatedAt: T(-60),
      expiresAt: T(-60),
    },
    {
      id: OPS.latestUnknown,
      kind: 'update',
      service: 't3-desktop',
      target: null,
      actionId: null,
      mode: 'scheduled',
      state: 'finished',
      phase: 'done',
      action: 'failed',
      reasonCode: 'apply-attempts-exhausted',
      requestedAt: T(-600),
      updatedAt: T(-598),
      expiresAt: null,
    },
    summarise(OP_CANCELLED),
    summarise(OP_UPDATE_DONE),
    summarise(OP_CYCLE),
    summarise(OP_ROLLED_BACK),
    summarise(OP_BOOT_REBOOTED),
    summarise(OP_INTERRUPTED),
  ],
    limit: 30,
    retained: 68,
    filter: { service: null, kind: null },
  },
});

add('logs.agent.json', {
  command: 'logs',
  variant: 'agent',
  schema: 'logs.schema.json',
  description: 'The tail of legionctl.log, with the timestamp and the command already split off the line. Every client would otherwise write the same regular expression, and one of them would get it wrong.',
  tags: ['diagnostics'],
}, {
  ...envelope(),
  source: 'agent',
  opId: null,
  path: '/home/x1f4r/.legion-control/legionctl.log',
  requested: 100,
  returned: 6,
  truncated: true,
  lines: [
    { at: T(-720), line: 'lock taken for cycle', command: 'cycle' },
    { at: T(-720, 2), line: 'demo skipped: automatic updates are off for this service', command: 'cycle' },
    { at: T(-718), line: 'cycle done: 1 updated, 2 deferred', command: 'cycle' },
    { at: T(-30), line: 'run sunshine-restart', command: 'run' },
    { at: T(0), line: 'lock taken for update t3 | busy gate forced past: 1 turn running', command: 'update' },
    { at: null, line: 'npm warn deprecated inflight@1.0.6: this package is no longer supported', command: null },
  ],
});

add('logs.operation.json', {
  command: 'logs',
  variant: 'operation',
  schema: 'logs.schema.json',
  description: 'One operation’s own log, which is the same list the record carries and the thing to show beside a failed update.',
  tags: ['diagnostics', 'operations'],
}, {
  ...envelope(),
  source: 'operation',
  opId: OPS.rolledBack,
  path: null,
  requested: 100,
  returned: 3,
  truncated: false,
  lines: OP_ROLLED_BACK.log.map((entry) => ({ at: entry.at, line: entry.line, command: 'update' })),
});

// -- doctor / bundle --------------------------------------------------------

const DOCTOR_OK = {
  ...envelope(),
  deep: false,
  checks: [
    { id: 'config.parse', level: 'ok', summary: 'config.json parses', detail: '4.1 kB, read at 2026-09-05T14:00:00.000Z', fix: null },
    { id: 'config.validate', level: 'ok', summary: '3 services, 1 boot target, 2 actions', detail: null, fix: null },
    { id: 'base.writable', level: 'ok', summary: '/home/x1f4r/.legion-control is writable', detail: null, fix: null },
    { id: 'node.version', level: 'ok', summary: 'node 24.9.0', detail: 'the agent needs 22 or newer', fix: null },
    { id: 'node.sqlite', level: 'ok', summary: 'node:sqlite is available', detail: 'needed by the t3-sqlite busy probe', fix: null },
    { id: 'npm.available', level: 'ok', summary: 'npm 11.6.0', detail: null, fix: null },
    { id: 'npm.prefix', level: 'ok', summary: 'global prefix /home/x1f4r/.local', detail: null, fix: null },
    { id: 'service.t3.process', level: 'ok', summary: 't3-code.service is active since 2026-09-03T14:10:00.000Z', detail: null, fix: null },
    { id: 'service.t3.health', level: 'ok', summary: 't3 answered 200 on 8081 in 8 ms', detail: null, fix: null },
    { id: 'service.t3.busy', level: 'ok', summary: 'the t3-sqlite probe read the state database', detail: '1 turn running', fix: null },
    { id: 'service.t3.relay', level: 'ok', summary: 'cloudflared is running', detail: null, fix: null },
    { id: 'scheduler.present', level: 'ok', summary: 'legion-control-cycle.timer is enabled', detail: 'next run 2026-09-06T02:00:00.000Z', fix: null },
    { id: 'scheduler.command', level: 'ok', summary: 'the timer runs this agent with "cycle"', detail: null, fix: null },
    { id: 'scheduler.lastRun', level: 'ok', summary: 'last run 2026-09-05T02:02:00.000Z, action cycled', detail: null, fix: null },
    { id: 'privileges.sudo', level: 'ok', summary: 'passwordless sudo for the four narrowed commands', detail: 'systemctl reboot, systemctl suspend, efibootmgr, grub-reboot', fix: null },
    { id: 'privileges.boot', level: 'ok', summary: 'efibootmgr can read and set BootNext', detail: null, fix: null },
    { id: 'sleep.tool', level: 'ok', summary: 'systemctl suspend is available', detail: null, fix: null },
    { id: 'locks', level: 'ok', summary: 'no stale locks', detail: 'op.lock is held by pid 24817, which is alive', fix: null },
    { id: 'ops.dir', level: 'ok', summary: '68 operation records, oldest 2026-08-22T09:14:00.000Z', detail: 'pruned at 200 records or 30 days', fix: null },
    { id: 'controller.copy', level: 'ok', summary: 'revision 12 from mac, 3d7251f8', detail: null, fix: null },
    { id: 'session.restricted', level: 'ok', summary: 'this session is not restricted', detail: null, fix: null },
    { id: 'clock', level: 'ok', summary: 'the clock is within 0.4 s of the client', detail: null, fix: null },
  ],
  counts: { ok: 22, warn: 0, fail: 0 },
  elapsedMs: 1840,
};

add('doctor.ok.json', {
  command: 'doctor',
  variant: 'ok',
  schema: 'doctor.schema.json',
  description: 'A healthy machine, every check named. The list is worth having when it all passes too: it is the only place that says what the agent is actually relying on.',
  tags: ['diagnostics'],
}, DOCTOR_OK);

add('doctor.deep-failures.json', {
  command: 'doctor',
  variant: 'deep-failures',
  schema: 'doctor.schema.json',
  description: 'A machine with real problems, including the two network checks only --deep runs. Every check that is not ok carries a fix in the imperative; a diagnosis nobody can act on is a complaint.',
  tags: ['diagnostics', 'deep'],
}, {
  ...envelope({ ok: false, message: '3 checks failed and 2 warned' }),
  deep: true,
  checks: [
    { id: 'config.parse', level: 'ok', summary: 'config.json parses', detail: null, fix: null },
    {
      id: 'config.validate',
      level: 'warn',
      summary: 'edge has no busy probe',
      detail: 'a service with no busy key counts as unmonitored, which blocks every disruptive action on this machine',
      fix: 'add a busy probe for edge, or set its busy.type to "none" if it is never busy',
    },
    { id: 'base.writable', level: 'ok', summary: '/home/x1f4r/.legion-control is writable', detail: null, fix: null },
    { id: 'node.version', level: 'ok', summary: 'node 24.9.0', detail: null, fix: null },
    {
      id: 'node.sqlite',
      level: 'fail',
      summary: 'node:sqlite is not available in this node build',
      detail: 'the t3 busy probe is type t3-sqlite and cannot run, so t3 counts as busy and nothing will update',
      fix: 'install node 22.5 or newer from NodeSource, or change the t3 busy probe to a command probe',
    },
    { id: 'npm.available', level: 'ok', summary: 'npm 11.6.0', detail: null, fix: null },
    { id: 'npm.prefix', level: 'ok', summary: 'global prefix /home/x1f4r/.local', detail: null, fix: null },
    { id: 'service.t3.process', level: 'ok', summary: 't3-code.service is active', detail: null, fix: null },
    {
      id: 'service.t3.health',
      level: 'warn',
      summary: 't3 answered 503 on 8081',
      detail: 'the process is up and the service is not ready',
      fix: 'look at journalctl --user -u t3-code.service for what it is waiting on',
    },
    {
      id: 'service.demo.latest',
      level: 'fail',
      summary: 'the latest version of demo cannot be read',
      detail: 'the configured latestVersion command reads ~/demo/latest, which does not exist',
      fix: 'create ~/demo/latest, or remove latestVersion and configure a verify command instead',
    },
    {
      id: 'service.t3.endpoint',
      level: 'ok',
      summary: 't3 answered 200 through its public endpoint in 214 ms',
      detail: 'https://t3.example.net',
      fix: null,
    },
    {
      id: 'scheduler.present',
      level: 'fail',
      summary: 'no scheduler entry runs this agent',
      detail: 'legion-control-cycle.timer is not enabled, so nothing updates on its own',
      fix: 'run the installer again, or: systemctl --user enable --now legion-control-cycle.timer',
    },
    {
      id: 'scheduler.command',
      level: 'warn',
      summary: 'the timer that exists still runs "update", not "cycle"',
      detail: 'a 2.x scheduler updates only the first service and ignores the queue',
      fix: 'run the installer again to rewrite the timer to "cycle"',
    },
    { id: 'locks', level: 'ok', summary: 'no stale locks', detail: null, fix: null },
    { id: 'ops.dir', level: 'ok', summary: '68 operation records', detail: null, fix: null },
    { id: 'controller.copy', level: 'ok', summary: 'revision 12 from mac', detail: null, fix: null },
    { id: 'session.restricted', level: 'ok', summary: 'this session is not restricted', detail: null, fix: null },
    {
      id: 'clock',
      level: 'ok',
      summary: 'the clock is within 0.4 s of the client',
      detail: null,
      fix: null,
    },
  ],
  counts: { ok: 12, warn: 3, fail: 3 },
  elapsedMs: 5210,
});

add('bundle.json', {
  command: 'bundle',
  variant: 'default',
  schema: 'bundle.schema.json',
  description: 'Everything needed to work out what is wrong with a machine, in one reply. Each part is the whole reply of the command it is named after, so a client decodes it with the decoder it already has, and every argv value that looked like a key path or a token is the string "<redacted>".',
  tags: ['diagnostics', 'redaction'],
}, {
  ...envelope(),
  generatedAt: T(1),
  doctor: DOCTOR_OK,
  status: status({
    operations: {
      running: [summarise(OP_UPDATE_RUNNING)],
      queued: [summarise(OP_RESTART_QUEUED)],
      recent: [summarise(OP_RUN), summarise(OP_UPDATE_DONE), summarise(OP_CYCLE)],
    },
  }),
  history: {
    ...envelope(),
    operations: [summarise(OP_UPDATE_RUNNING), summarise(OP_RUN), summarise(OP_UPDATE_DONE)],
    limit: 200,
    returned: 3,
    retained: 68,
    filter: { service: null, kind: null },
  },
  logs: {
    ...envelope(),
    source: 'agent',
    opId: null,
    path: '/home/x1f4r/.legion-control/legionctl.log',
    requested: 200,
    returned: 2,
    truncated: true,
    lines: [
      { at: T(-718), line: 'cycle done: 1 updated, 2 deferred', command: 'cycle' },
      { at: T(0), line: 'lock taken for update t3', command: 'update' },
    ],
  },
  config: {
    system: LINUX,
    autoUpdate: true,
    updates: { automatic: true, pauseUntil: null, maintenanceWindows: WINDOWS_NIGHTLY },
    services: [
      {
        id: 't3',
        name: 'T3 Code',
        kind: 'npm',
        package: 't3',
        channel: 'nightly',
        process: { type: 'systemd-user', unit: 't3-code.service' },
        busy: { type: 't3-sqlite', home: '/home/x1f4r/.t3', staleHours: 12 },
        health: { type: 'http', port: 8081, path: '/health' },
        deploy: { identityFile: '<redacted>', host: 'atlas' },
      },
    ],
  },
  redacted: ['services[0].deploy.identityFile'],
});
addDocument('controller-document.sites-helpers.json', {
  variant: 'sites-helpers',
  schema: 'controller-document.schema.json',
  description: 'A valid setup with two sites, an ordered helper list, dual-boot endpoint hints and a restricted system. The two sites carry the SAME private subnet on purpose: that is what two homes behind stock routers look like, and it is why a prefix match is a hint rather than proof of where a device is.',
  tags: ['controller', 'sites', 'wake', 'peer'],
}, CONTROLLER_DOC);

addDocument('controller-document.minimal.json', {
  variant: 'minimal',
  schema: 'controller-document.schema.json',
  description: 'The smallest document that is still one: a version, one machine, and nothing else. Everything the peer amendment adds is optional, so a setup written before any of it still loads.',
  tags: ['controller', 'compatibility'],
}, {
  version: 1,
  machines: [{ id: 'pi', name: 'Atlas', endpoints: [{ host: '192.168.178.40', user: 'x1f4r' }] }],
});

addDocument('controller-document.unknown-keys.json', {
  variant: 'unknown-keys',
  schema: 'controller-document.schema.json',
  description: 'A document carrying keys this version has never heard of, at the top level, on a machine and on an endpoint. It is VALID, and every editor must write those keys back unchanged. An editor that round-trips through its typed model drops them, and because the hash is over the whole document the deletion then looks like a deliberate edit and propagates to every peer.',
  tags: ['controller', 'forward-compatibility', 'peer'],
}, {
  version: 1,
  controller: { id: SETUP_ID, revision: 13, lineage: [H_CURRENT] },
  futureTopLevelKey: { addedBy: '1.4', keep: true },
  machines: [
    {
      id: 'pi',
      name: 'Atlas',
      futureMachineKey: ['anything', 'at', 'all'],
      endpoints: [{ id: 'lan', host: '192.168.178.40', user: 'x1f4r', futureEndpointKey: 42 }],
    },
  ],
});

addDocument('controller-document.invalid-helper-cycle.json', {
  variant: 'invalid-helper-cycle',
  schema: 'controller-document.schema.json',
  expect: 'invalid',
  description: 'INVALID. Two machines name each other as their wake helper, so waking either one requires the other to be awake already. The schema alone cannot see this; contract/validate.mjs walks the helper graph, and every client editor must refuse to save it for the same reason.',
  tags: ['controller', 'wake', 'invalid'],
}, {
  version: 1,
  controller: { id: SETUP_ID, revision: 1, lineage: [] },
  machines: [
    {
      id: 'legion',
      name: 'Legion 7',
      wake: { mac: 'AA:BB:CC:DD:EE:FF', helper: { machine: 'tower', action: 'wake-legion' }, helpers: [{ machine: 'tower', action: 'wake-legion' }] },
    },
    {
      id: 'tower',
      name: 'Tower',
      wake: { mac: '11:22:33:44:55:66', helper: { machine: 'legion', action: 'wake-tower' }, helpers: [{ machine: 'legion', action: 'wake-tower' }] },
    },
  ],
});

addDocument('controller-document.invalid-unknown-site.json', {
  variant: 'invalid-unknown-site',
  schema: 'controller-document.schema.json',
  expect: 'invalid',
  description: 'INVALID. A machine names a site that sites[] does not define, which would silently drop it out of every on-site wake decision.',
  tags: ['controller', 'sites', 'invalid'],
}, {
  version: 1,
  controller: { id: SETUP_ID, revision: 1, lineage: [] },
  sites: [{ id: 'attic', name: 'Attic house', lanPrefixes: ['192.168.178.'] }],
  machines: [{ id: 'tower', name: 'Tower', site: 'flat' }],
});

addDocument('controller-document.invalid-self-helper.json', {
  variant: 'invalid-self-helper',
  schema: 'controller-document.schema.json',
  expect: 'invalid',
  description: 'INVALID. A machine is its own wake helper, which asks a sleeping machine to wake itself.',
  tags: ['controller', 'wake', 'invalid'],
}, {
  version: 1,
  machines: [
    { id: 'tower', name: 'Tower', wake: { mac: '11:22:33:44:55:66', helper: { machine: 'tower', action: 'wake-tower' }, helpers: [{ machine: 'tower', action: 'wake-tower' }] } },
  ],
});

addDocument('controller-document.invalid-helper-mismatch.json', {
  variant: 'invalid-helper-mismatch',
  schema: 'controller-document.schema.json',
  expect: 'invalid',
  description: 'INVALID. `helper` and `helpers[0]` name different helpers, so a 1.2 client and a 1.3 client would try different machines from the same document. An editor that writes helpers must write helper = helpers[0].',
  tags: ['controller', 'wake', 'compatibility', 'invalid'],
}, {
  version: 1,
  machines: [
    { id: 'pi', name: 'Atlas' },
    { id: 'legion', name: 'Legion 7' },
    {
      id: 'tower',
      name: 'Tower',
      wake: {
        mac: '11:22:33:44:55:66',
        helper: { machine: 'legion', action: 'wake-tower' },
        helpers: [{ machine: 'pi', action: 'wake-tower' }],
      },
    },
  ],
});

// -- private bindings --------------------------------------------------------

addDocument('bindings.desktop-self.json', {
  variant: 'desktop-self',
  schema: 'bindings.schema.json',
  description: 'What one device knows about itself: which machine it is, how to run its own agent without ssh, where its keys live, and which site the user says it is on. None of it is publishable, and none of it is a key — only where the keys are.',
  tags: ['bindings', 'peer', 'self-control'],
}, {
  deviceName: "Robert's tower",
  self: { machine: 'tower', system: 'linux' },
  localAgent: { argv: ['node', '/home/x1f4r/.legion-control/agent/src/index.mjs'] },
  identityFile: '~/.ssh/legion-control_ed25519',
  knownHostsFile: '~/.config/legion-control/known_hosts',
  currentSite: 'flat',
  machines: {
    legion: { identityFile: '~/.ssh/id_legion', sshAlias: 'legion-win-lan' },
    pi: { sshAlias: 'atlas' },
  },
});

addDocument('bindings.phone-minimal.json', {
  variant: 'phone-minimal',
  schema: 'bindings.schema.json',
  description: 'The phone. It never sets self or localAgent, because it cannot run a service — which the app says outright instead of showing an empty section. It has exactly the same right to edit and publish the setup as every other device.',
  tags: ['bindings', 'android', 'peer'],
}, {
  deviceName: 'Pixel 9',
  identityFile: 'legion-control-phone-key',
});

addDocument('bindings.invalid-local-agent-shell.json', {
  variant: 'invalid-local-agent-shell',
  schema: 'bindings.schema.json',
  expect: 'invalid',
  description: 'INVALID. There is no localAgent.shell. Local execution spawns the argv directly, so wrapping it in cmd or powershell would add a quoting layer between the client and the agent with no ssh anywhere on the path to need one.',
  tags: ['bindings', 'invalid'],
}, {
  deviceName: "Robert's tower",
  self: { machine: 'tower' },
  localAgent: { argv: ['node', 'C:/Users/x1f4r/.legion-control/agent/src/index.mjs'], shell: 'cmd' },
});

// -- config read and set -----------------------------------------------------

const CONTROLLER_META = {
  id: SETUP_ID,
  revision: 12,
  updatedAt: T(-4200),
  source: 'mac',
  lineage: LINEAGE,
  device: "Robert's MacBook Pro",
  hash: H_CURRENT,
  bytes: CONTROLLER_DOC_SIZE,
};

add('config.read.json', {
  command: 'config',
  variant: 'read',
  schema: 'config-read.schema.json',
  description: 'The setup a machine carries, with the lineage that decides whether a client is behind it. The agent stores bytes and reads only the controller block; the document comes back as it was given.',
  tags: ['controller', 'peer'],
}, {
  ...envelope(),
  controller: CONTROLLER_DOC,
  hash: H_CURRENT,
  bytes: CONTROLLER_DOC_SIZE,
  meta: CONTROLLER_META,
});

add('config.read-empty.json', {
  command: 'config',
  variant: 'read-empty',
  schema: 'config-read.schema.json',
  description: 'A machine that holds no setup yet. A hash of nothing means "ready for one and has none", which is a different thing from a hash that disagrees, and the first push to it is accepted without a question.',
  tags: ['controller'],
}, {
  ...envelope(),
  controller: null,
  hash: null,
  bytes: 0,
  meta: null,
});

add('config.meta-empty.json', {
  command: 'config',
  variant: 'meta-empty',
  schema: 'config-meta.schema.json',
  description: 'config meta on a machine that carries nothing. Every field is honestly null and the lineage is empty, which is a different answer from "the file is unreadable" and lets the first push through without a question.',
  tags: ['controller', 'peer'],
}, {
  ...envelope(),
  meta: { id: null, revision: 0, updatedAt: null, source: null, lineage: [], device: null, hash: null, bytes: 0 },
  hash: null,
});

add('config.set.bad-argument.json', {
  command: 'config set',
  variant: 'bad-argument',
  schema: 'config-set.schema.json',
  description: 'The --controller-id and --revision flags do not match the controller block inside the document on stdin. Refused before anything is written: the flags are what the agent checks the document against, and letting them disagree would let a client label somebody else\'s document as its own.',
  tags: ['controller', 'grammar'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'bad-argument',
    message: '--revision 13 does not match controller.revision 12 in the document on stdin; nothing was written',
  }),
  hash: null,
  bytes: 0,
  id: SETUP_ID,
  revision: 13,
  divergent: false,
  current: CONTROLLER_META,
});

add('config.set.stored.json', {
  command: 'config set',
  variant: 'stored',
  schema: 'config-set.schema.json',
  description: 'The first setup a machine has ever held. Nothing to descend from, so it is simply stored.',
  tags: ['controller', 'peer'],
}, {
  ...envelope({ message: `stored revision 12 of setup-3f9c2a71, ${CONTROLLER_DOC_SIZE} bytes` }),
  action: 'stored',
  hash: H_CURRENT,
  bytes: CONTROLLER_DOC_SIZE,
  id: SETUP_ID,
  revision: 12,
  source: 'mac',
  lineage: LINEAGE,
  device: "Robert's MacBook Pro",
});

add('config.set.fast-forward.json', {
  command: 'config set',
  variant: 'fast-forward',
  schema: 'config-set.schema.json',
  description: 'The machine held H_PARENT and the pushed document lists it as an ancestor, so the machine is behind and the push descends from what it has. This is the only kind of automatic write there is: strictly along descent, which is acyclic, so two peers can never take turns overwriting each other.',
  tags: ['controller', 'peer', 'lineage'],
}, {
  ...envelope({ message: `fast-forwarded from 1c680fab to ${H_CURRENT.slice(0, 8)}, revision 12, ${CONTROLLER_DOC_SIZE} bytes` }),
  action: 'stored',
  hash: H_CURRENT,
  bytes: CONTROLLER_DOC_SIZE,
  id: SETUP_ID,
  revision: 12,
  source: 'phone',
  lineage: LINEAGE,
  device: 'Pixel 9',
});

add('config.set.noop.json', {
  command: 'config set',
  variant: 'noop',
  schema: 'config-set.schema.json',
  description: 'The same bytes the machine already holds. Nothing is written and the answer is success, which is what makes a push safe to retry after a link drops mid-reply: the retry is byte-identical and lands here.',
  tags: ['controller', 'idempotency'],
}, {
  ...envelope({ message: 'this machine already holds 3d7251f8; nothing was written' }),
  action: 'noop',
  hash: H_CURRENT,
  bytes: CONTROLLER_DOC_SIZE,
  id: SETUP_ID,
  revision: 12,
  source: 'mac',
  lineage: LINEAGE,
});

add('config.set.stale-revision.json', {
  command: 'config set',
  variant: 'stale-revision',
  schema: 'config-set.schema.json',
  description: 'The pushed hash appears in the lineage the machine holds, which says plainly that the pusher is an ancestor: it is behind, not divergent. divergent is false, so the client fetches and catches up without asking anybody anything.',
  tags: ['controller', 'peer', 'lineage'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'stale-revision',
    message: 'this machine holds 3d7251f8 (revision 12), which descends from the 1c680fab you pushed; fetch it rather than pushing over it',
  }),
  hash: null,
  bytes: 0,
  id: SETUP_ID,
  revision: 11,
  divergent: false,
  current: CONTROLLER_META,
});

add('config.set.conflict-divergent.json', {
  command: 'config set',
  variant: 'conflict-divergent',
  schema: 'config-set.schema.json',
  description: 'Same setup, and neither document descends from the other: two people edited the same revision while apart. Nothing is written and nothing is chosen. This is the case revision numbers cannot see — both sides may say 13 — and it is exactly the case where a fast-forward rule would have destroyed one of the two edits.',
  tags: ['controller', 'peer', 'lineage', 'conflict'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'controller-conflict',
    message: 'this machine holds 3d7251f8 revision 12 from mac; the document offered is revision 12 and neither descends from the other, so both edits are still here and somebody has to merge them',
  }),
  hash: null,
  bytes: 0,
  id: SETUP_ID,
  revision: 12,
  divergent: true,
  current: CONTROLLER_META,
});

add('config.set.conflict-other-id.json', {
  command: 'config set',
  variant: 'conflict-other-id',
  schema: 'config-set.schema.json',
  description: 'A different setup entirely. Refused without --replace, because taking it would drop every machine the held setup describes, and no client is allowed to make that choice on its own.',
  tags: ['controller', 'peer', 'conflict'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'controller-conflict',
    message: 'this machine holds setup setup-3f9c2a71 and the document offered is setup setup-8b04d193; --replace is needed, and somebody has to decide',
  }),
  hash: null,
  bytes: 0,
  id: 'setup-8b04d193-6c27-4a51-83ef-90b7d2c46e15',
  revision: 4,
  divergent: true,
  current: CONTROLLER_META,
});

add('config.set.replaced.json', {
  command: 'config set',
  variant: 'replaced',
  schema: 'config-set.schema.json',
  description: 'The same push again with --replace, after a person answered the question. --replace is authority replacement, never a retry flag, and no client ever sends it on its own.',
  tags: ['controller', 'peer'],
}, {
  ...envelope({ message: 'setup setup-3f9c2a71 replaced by setup-8b04d193 revision 4, 198 bytes' }),
  action: 'replaced',
  hash: H_OTHER_BRANCH,
  bytes: 198,
  id: 'setup-8b04d193-6c27-4a51-83ef-90b7d2c46e15',
  revision: 4,
  source: 'phone',
  lineage: [],
  device: 'Pixel 9',
  replaced: true,
});

add('config.set.locked.json', {
  command: 'config set',
  variant: 'operation-in-progress',
  schema: 'config-set.schema.json',
  description: 'A setup push that arrived while an update held the machine lock. Refused, because a document rewritten under a running update could be read half-written by that update. Nothing about the document is wrong, so the reply names the operation rather than the held copy, and the client retries when the operation finishes.',
  tags: ['controller', 'operation-lock', 'peer'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'operation-in-progress',
    message: 'an update of t3 has held the lock since 2026-09-05T14:00:01.000Z; the setup was not changed, and this push can be retried when it finishes',
  }),
  hash: null,
  bytes: 0,
  id: SETUP_ID,
  revision: 13,
  conflict: {
    opId: OPS.updateRunning,
    kind: 'update',
    service: 't3',
    phase: 'installing',
    startedAt: T(0, 1),
  },
});

add('config.meta.json', {
  command: 'config',
  variant: 'meta-with-lineage',
  schema: 'config-meta.schema.json',
  description: 'Identity, revision and lineage without the document: one line instead of a megabyte. This is the extra round trip a client makes only when the hashes differ and status alone cannot decide which side is ahead.',
  tags: ['controller', 'peer', 'lineage'],
}, {
  ...envelope(),
  meta: CONTROLLER_META,
  hash: H_CURRENT,
});

// -- policy -----------------------------------------------------------------

add('policy.system.json', {
  command: 'policy',
  variant: 'system',
  schema: 'policy.schema.json',
  description: 'The machine-wide policy. Three separate ideas that used to be one boolean: whether the scheduler may act, whether it has been told to leave things alone for a while, and the hours it may work in. A manual request is none of them.',
  tags: ['policy'],
}, {
  ...envelope(),
  service: null,
  updates: {
    automatic: true,
    pauseUntil: null,
    maintenanceWindows: WINDOWS_NIGHTLY,
    inWindowNow: false,
    nextWindow: T(720),
    lastCycle: { opId: OPS.cycle, at: T(-718), action: 'cycled' },
  },
  services: [
    {
      id: 't3',
      name: 'T3 Code',
      updates: {
        automatic: true,
        inherited: true,
        pauseUntil: null,
        maintenanceWindows: null,
        eligibleNow: false,
        deferredReason: 'busy',
        inheritedKeys: { automatic: true, pauseUntil: true, maintenanceWindows: true },
        inWindowNow: false,
        nextWindow: T(720),
      },
    },
    {
      id: 'demo',
      name: 'Demo',
      updates: {
        automatic: false,
        inherited: false,
        pauseUntil: null,
        maintenanceWindows: null,
        eligibleNow: false,
        deferredReason: 'policy-off',
        inheritedKeys: { automatic: false, pauseUntil: true, maintenanceWindows: true },
        inWindowNow: false,
        nextWindow: T(720),
      },
    },
  ],
  autoUpdate: true,
  changed: [],
});

add('policy.service.json', {
  command: 'policy',
  variant: 'service',
  schema: 'policy.schema.json',
  description: 'One service’s policy after inheritance, with the key-by-key record of what came from the service and what came from the machine. A switch drawn without that shows the user a setting they never made.',
  tags: ['policy', 'inheritance'],
}, {
  ...envelope(),
  service: 'demo',
  updates: {
    automatic: false,
    inherited: false,
    pauseUntil: null,
    maintenanceWindows: null,
    eligibleNow: false,
    deferredReason: 'policy-off',
    inheritedKeys: { automatic: false, pauseUntil: true, maintenanceWindows: true },
    inWindowNow: false,
    nextWindow: T(720),
  },
  autoUpdate: true,
  changed: [],
});

add('policy.set-paused.json', {
  command: 'policy',
  variant: 'set-paused',
  schema: 'policy.schema.json',
  description: 'A pause applied to one service. Only the key that was touched is rewritten, and the reply is the effective policy afterwards, so the screen redraws from the answer rather than from what it hoped it did.',
  tags: ['policy'],
}, {
  ...envelope({ message: 'updates for t3 are paused until 2026-09-08T02:00:00.000Z' }),
  service: 't3',
  updates: {
    automatic: true,
    inherited: true,
    pauseUntil: '2026-09-08T02:00:00.000Z',
    maintenanceWindows: null,
    eligibleNow: false,
    deferredReason: 'policy-paused',
    inheritedKeys: { automatic: true, pauseUntil: false, maintenanceWindows: true },
    inWindowNow: false,
    nextWindow: T(720),
  },
  autoUpdate: true,
  changed: ['pauseUntil'],
});

add('auto-update.off.json', {
  command: 'auto-update',
  variant: 'off',
  schema: 'policy.schema.json',
  description: 'The 2.x switch, answering in the v3 shape. `autoUpdate` is still the plain boolean a 2.x client reads; turning the scheduler off never stops a manual update.',
  tags: ['policy', '2x-compatibility'],
}, {
  ...envelope({ message: 'automatic updates are off for this machine; a manual update still runs' }),
  service: null,
  updates: {
    automatic: false,
    pauseUntil: null,
    maintenanceWindows: WINDOWS_NIGHTLY,
    inWindowNow: false,
    nextWindow: T(720),
    lastCycle: { opId: OPS.cycle, at: T(-718), action: 'cycled' },
  },
  services: [
    {
      id: 't3',
      name: 'T3 Code',
      updates: {
        automatic: false,
        inherited: true,
        pauseUntil: null,
        maintenanceWindows: null,
        eligibleNow: false,
        deferredReason: 'policy-off',
        inheritedKeys: { automatic: true, pauseUntil: true, maintenanceWindows: true },
        inWindowNow: false,
        nextWindow: T(720),
      },
    },
    {
      id: 'demo',
      name: 'Demo',
      updates: {
        automatic: false,
        inherited: false,
        pauseUntil: null,
        maintenanceWindows: null,
        eligibleNow: false,
        deferredReason: 'policy-off',
        inheritedKeys: { automatic: false, pauseUntil: true, maintenanceWindows: true },
        inWindowNow: false,
        nextWindow: T(720),
      },
    },
  ],
  autoUpdate: false,
  changed: ['automatic'],
});

// -- self-update ------------------------------------------------------------

const KEY_FINGERPRINT = '8f2b6d41c0973ea5b8de10f47c26935ab0d84e7f1c53920684bfae7d3506c19b';

add('self-update.check.json', {
  command: 'self-update',
  variant: 'check',
  schema: 'self-update.schema.json',
  description: 'What is installed and whether the tarball the client carries is newer. Reads nothing but the running tree; nothing is staged and nothing is verified yet.',
  tags: ['self-update'],
}, {
  ...envelope({ message: 'agent 2.1.0 is installed, contract 2; the bundled agent is 3.0.0' }),
  action: 'checked',
  current: { version: '2.1.0', contract: 2 },
  staged: null,
  previous: null,
  manifest: null,
  selfTest: null,
  available: true,
  op: null,
});

add('self-update.installed.json', {
  command: 'self-update',
  variant: 'installed',
  schema: 'self-update.schema.json',
  description: 'A swap that happened. The signature was checked against the key this running agent was built with, every file hash matched, the staged tree passed its own self-test, and only then was anything renamed. The previous tree is kept.',
  tags: ['self-update', 'trust'],
}, {
  ...envelope({ message: 'agent 3.0.0 is installed and 2.1.0 is kept at /home/x1f4r/.legion-control/agent.prev' }),
  action: 'installed',
  current: { version: '3.0.0', contract: 3 },
  staged: null,
  previous: { version: '2.1.0', path: '/home/x1f4r/.legion-control/agent.prev' },
  manifest: {
    schema: 1,
    version: '3.0.0',
    contract: 3,
    files: 31,
    signatureVerified: true,
    keyFingerprint: KEY_FINGERPRINT,
  },
  selfTest: { ok: true, exitCode: 0, output: '{"ok":true,"contract":3,"agentVersion":"3.0.0","selfTest":{"ok":true}}' },
  op: OP_SELF_UPDATE,
});

add('self-update.signature-invalid.json', {
  command: 'self-update',
  variant: 'signature-invalid',
  schema: 'self-update.schema.json',
  description: 'The manifest is not signed by the release key. Nothing was extracted, nothing was staged, nothing ran. This check happens before the archive is unpacked, not after.',
  tags: ['self-update', 'trust'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'signature-invalid',
    message: 'MANIFEST.json.sig is not a valid signature over MANIFEST.json for the release key 8f2b6d41; nothing was extracted and the running agent is untouched',
  }),
  action: 'failed',
  current: { version: '2.1.0', contract: 2 },
  staged: null,
  previous: null,
  manifest: null,
  selfTest: null,
  op: null,
});

add('self-update.self-test-failed.json', {
  command: 'self-update',
  variant: 'self-test-failed',
  schema: 'self-update.schema.json',
  description: 'A properly signed tree that does not run on this machine. It was staged, it failed its own self-test, and nothing was swapped: the old tree is still the one in place.',
  tags: ['self-update', 'recovery'],
}, {
  ...envelope({
    ok: false,
    reasonCode: 'internal',
    message: 'the staged agent 3.0.0 failed its self-test (node:sqlite is not available in node 20.11.1), so nothing was swapped and 2.1.0 is still running',
  }),
  action: 'failed',
  current: { version: '2.1.0', contract: 2 },
  staged: null,
  previous: null,
  manifest: {
    schema: 1,
    version: '3.0.0',
    contract: 3,
    files: 31,
    signatureVerified: true,
    keyFingerprint: KEY_FINGERPRINT,
  },
  selfTest: {
    ok: false,
    exitCode: 1,
    output: "Error: Cannot find module 'node:sqlite'",
  },
  op: null,
});

add('self-update.rolled-back.json', {
  command: 'self-update', variant: 'rolled-back', schema: 'self-update.schema.json',
  description: 'A complete signed previous agent passed signature, file hashes and its own version/contract self-test before being restored; the outgoing signed build remains retained.',
  tags: ['self-update', 'recovery'],
}, {
  ...envelope({ message: 'rolled back to the previous signed agent 3.0.0; the outgoing build is kept at /home/x1f4r/.legion-control/agent.prev' }),
  action: 'rolled-back', current: { version: '3.0.0', contract: 3 }, staged: null,
  previous: { version: '3.0.0', path: '/home/x1f4r/.legion-control/agent.prev' },
  manifest: { schema: 1, version: '3.0.0', contract: 3, files: 31, signatureVerified: true, keyFingerprint: KEY_FINGERPRINT },
  selfTest: { ok: true, exitCode: 0, output: '{"ok":true,"contract":3,"agentVersion":"3.0.0","selfTest":{"ok":true}}' },
  op: { ...OP_SELF_UPDATE, id: OPS.selfRollback, target: `rollback:${'2'.repeat(64)}`, from: '3.0.0', to: '3.0.0',
    result: { ...OP_SELF_UPDATE.result, action: 'rolled-back', from: '3.0.0', to: '3.0.0', message: 'The previous signed build is restored.' } },
});

add('self-update.legacy-rollback-refused.json', {
  command: 'self-update', variant: 'legacy-rollback-refused', schema: 'self-update.schema.json',
  description: 'The retained 2.x tree is unsigned. It remains on disk for independently verified manual recovery but cannot be executed by the ordinary rollback command.',
  tags: ['self-update', 'trust', 'legacy'],
}, {
  ...envelope({ ok: false, reasonCode: 'signature-invalid', message: 'The previous agent is retained, but it is not a complete tree signed by the release key; restore it manually after independent verification.' }),
  action: 'failed', current: { version: '3.0.0', contract: 3 }, staged: null,
  previous: { version: '2.1.0', path: '/home/x1f4r/.legion-control/agent.prev' },
  manifest: null, selfTest: null, op: null,
});

// -- version / help ---------------------------------------------------------

add('version.json', {
  command: 'version',
  variant: 'default',
  schema: 'version.schema.json',
  description: 'Who is answering. The cheapest call there is, and the one a client makes to find out whether it is talking to contract 3 at all.',
  tags: ['handshake'],
}, {
  ...envelope(),
  node: '24.9.0',
  base: '/home/x1f4r/.legion-control',
  os: 'linux',
});

add('version.check.json', {
  command: 'version',
  variant: 'check',
  schema: 'version.schema.json',
  description: '--check, which is the self-test a staged tree has to pass before self-update swaps it in: every module loaded, and the contract it implements printed.',
  tags: ['self-update'],
}, {
  ...envelope(),
  node: '24.9.0',
  base: '/home/x1f4r/.legion-control',
  os: 'linux',
  selfTest: { ok: true, modules: 18, failed: [] },
});

add('help.json', {
  command: 'help',
  variant: 'default',
  schema: 'help.schema.json',
  description: 'The command table, with the flags each command takes and whether it changes anything. The `mutates` flag is the one a restricted dispatcher reads.',
  tags: ['handshake'],
}, {
  ...envelope(),
  usage: 'node /home/x1f4r/.legion-control/agent/src/index.mjs <command> [args] [--flags]',
  commands: [
    { name: 'status', flags: ['--budget-ms <ms>'], mutates: false, summary: 'bounded snapshot with timing and partial flags' },
    { name: 'busy', flags: ['--budget-ms <ms>'], mutates: false, summary: 'aggregate busy plus per-service evidence' },
    { name: 'update', flags: ['--service <id>', '--force', '--op <id>', '--detach', '--when-idle', '--expires <dur>'], mutates: true, summary: 'manual update of one service' },
    { name: 'restart', flags: ['--service <id>', '--force', '--op <id>', '--detach', '--when-idle', '--expires <dur>'], mutates: true, summary: 'restart one service' },
    { name: 'boot', flags: ['--force', '--no-reboot', '--op <id>', '--when-idle', '--expires <dur>'], mutates: true, summary: 'arm a boot target and reboot into it' },
    { name: 'sleep', flags: ['--force', '--op <id>', '--when-idle', '--expires <dur>'], mutates: true, summary: 'suspend this machine' },
    { name: 'run', flags: ['--force', '--op <id>', '--detach', '--when-idle', '--expires <dur>'], mutates: true, summary: 'run a configured action' },
    { name: 'cycle', flags: ['--op <id>', '--dry-run'], mutates: true, summary: 'the scheduled maintenance cycle over every eligible service' },
    { name: 'policy', flags: ['--service <id>'], mutates: false, summary: 'read the effective update policy; "policy set" reads a JSON patch from stdin' },
    { name: 'auto-update', flags: ['--service <id>'], mutates: true, summary: 'on, off, pause <dur>, resume' },
    { name: 'op', flags: ['--wait <seconds>'], mutates: false, summary: 'one operation record; --wait long-polls until it finishes' },
    { name: 'cancel', flags: [], mutates: true, summary: 'cancel a queued operation' },
    { name: 'history', flags: ['--limit <n>', '--service <id>', '--kind <kind>'], mutates: false, summary: 'operation summaries, newest first' },
    { name: 'logs', flags: ['--lines <n>', '--op <id>'], mutates: false, summary: 'tail of legionctl.log, or one operation log' },
    { name: 'doctor', flags: ['--service <id>', '--deep'], mutates: false, summary: 'preflight and diagnosis checks' },
    { name: 'bundle', flags: [], mutates: false, summary: 'doctor, status, history, logs and a redacted config in one reply' },
    { name: 'config', flags: ['--controller-id <id>', '--revision <n>', '--replace'], mutates: true, summary: 'the stored controller document; "config set" reads one from stdin, "config meta" reads its identity' },
    { name: 'self-update', flags: ['--from <path>', '--install', '--rollback', '--check', '--stdin'], mutates: true, summary: 'signed agent replacement, keeping the previous tree' },
    { name: 'version', flags: ['--check'], mutates: false, summary: 'the agent version, and with --check the self-test' },
    { name: 'help', flags: [], mutates: false, summary: 'this list' },
  ],
});

// -- errors -----------------------------------------------------------------

function error(file, variant, description, tags, payload) {
  add(file, { command: payload.command ?? 'status', variant, schema: 'error.schema.json', description, tags }, payload);
}

error('error.bad-argument.json', 'bad-argument',
  'A token outside the argv grammar. Every token a client sends and every value the agent accepts matches ^[A-Za-z0-9][A-Za-z0-9._:@/\\\\~=+-]*$: no spaces, no quotes, no dollar, no semicolon. Refusing here is what keeps an id from ever reaching a shell.',
  ['grammar', 'security'], {
    ...envelope({
      ok: false,
      reasonCode: 'bad-argument',
      message: '--service "t3; rm -rf ~" is not a valid value: it must match ^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$',
    }),
    command: 'update',
    argument: '--service',
    accepts: ['t3', 'demo', 'edge'],
    error: '--service "t3; rm -rf ~" is not a valid value',
  });

error('error.unknown-service.json', 'unknown-service',
  'A service this machine does not have. The reply names the ones it does, so the app can correct itself rather than showing a dead button.',
  ['reason-codes'], {
    ...envelope({
      ok: false,
      reasonCode: 'unknown-service',
      message: 'this machine has no service called "t3-desktop"; it has t3, demo and edge',
    }),
    command: 'update',
    argument: '--service',
    accepts: ['t3', 'demo', 'edge'],
    error: 'unknown service: t3-desktop',
  });

error('error.unknown-target.json', 'unknown-target',
  'A boot target that is not configured. A machine with no other system to point at is a different answer again, and both are better than reaching efibootmgr and returning whatever it says.',
  ['reason-codes', 'boot'], {
    ...envelope({
      ok: false,
      reasonCode: 'unknown-target',
      message: 'this machine has no boot target called "macos"; it can boot into windows',
    }),
    command: 'boot',
    argument: 'macos',
    accepts: ['windows'],
    error: 'unknown boot target: macos',
  });

error('error.unknown-action.json', 'unknown-action',
  'An action id nobody configured.',
  ['reason-codes'], {
    ...envelope({
      ok: false,
      reasonCode: 'unknown-action',
      message: 'this machine has no action called "reindex"; it has sunshine-restart and flush-cache',
    }),
    command: 'run',
    argument: 'reindex',
    accepts: ['sunshine-restart', 'flush-cache'],
    error: 'unknown action: reindex',
  });

error('error.config-invalid.json', 'config-invalid',
  'Every mutation is refused while the configuration does not load. The agent will not guess at settings, and it will not fall back to defaults that might switch on automatic updates somebody had turned off.',
  ['config', 'fail-closed'], {
    ...envelope({
      ok: false,
      reasonCode: 'config-invalid',
      message: 'config.json does not parse, so nothing on this machine will be changed until it does; status still works and reads from config.last-good.json',
    }),
    command: 'update',
    argument: null,
    problems: [
      {
        level: 'error',
        path: 'config.json',
        message: 'the file does not parse: Unexpected token } in JSON at position 812',
        fix: 'fix the trailing comma at line 34, or delete config.json to start from defaults',
      },
    ],
    error: 'config.json does not parse',
  });

error('error.config-missing.json', 'config-missing',
  'No configuration at all, on a command that needs one. Reads answer from inert defaults; a mutation has nothing to act on and says so.',
  ['config'], {
    ...envelope({
      ok: false,
      reasonCode: 'config-missing',
      message: 'there is no /home/x1f4r/.legion-control/config.json, so this machine has no services to update',
    }),
    command: 'update',
    argument: null,
    error: 'no config.json',
  });

error('error.restricted.json', 'restricted',
  'A restricted key asked for something outside the command table. The dispatcher parses a limited argv grammar and never hands a string to a shell, so this is a refusal and not a partially executed command.',
  ['security', 'dispatcher'], {
    ...envelope({
      ok: false,
      reasonCode: 'restricted',
      message: 'this key may run status, busy, update, restart, boot, sleep, run, cycle, op, cancel, history, logs, doctor, bundle, config, policy and version on this agent, and nothing else; "ls" was refused',
    }),
    command: null,
    argument: 'ls',
    accepts: ['status', 'busy', 'update', 'restart', 'boot', 'sleep', 'run', 'cycle', 'op', 'cancel', 'history', 'logs', 'doctor', 'bundle', 'config', 'policy', 'version'],
    error: 'restricted session: "ls" is not an allowed command',
  });

error('error.unsupported-platform.json', 'unsupported-platform',
  'A host this agent was never written for. Refused early and clearly, rather than several layers down inside a probe.',
  ['platform'], {
    ...envelope({
      ok: false,
      system: { id: 'freebsd', name: 'FreeBSD' },
      reasonCode: 'unsupported-platform',
      message: 'legionctl runs on linux, windows and mac only; this host reports "freebsd"',
    }),
    command: 'status',
    argument: null,
    error: 'legionctl runs on linux, windows and mac only; this host reports "freebsd"',
  });

error('error.internal.json', 'internal',
  'Something the agent did not expect. Still one JSON object on stdout, still exit 1, and the stack line goes to the log rather than into the reply.',
  ['failure'], {
    ...envelope({
      ok: false,
      reasonCode: 'internal',
      message: 'the agent failed while writing the operation record: ENOSPC: no space left on device, write',
    }),
    command: 'update',
    argument: null,
    error: 'ENOSPC: no space left on device, write',
  });

// -- legacy 2.x -------------------------------------------------------------

const LEGACY_BUSY = {
  busy: true,
  unknown: false,
  reason: '1 turn running',
  checkedAt: T(0),
  error: null,
  runningTurns: 1,
  pendingTurns: 0,
  pendingApprovals: 0,
  staleTurns: 0,
  staleApprovals: 0,
  threads: [],
  threadsTruncated: 0,
};

const LEGACY_SERVICE = {
  id: 't3',
  name: 'T3 Code',
  kind: 'npm',
  installed: '0.0.36-nightly.20260904',
  latest: '0.0.37-nightly.20260905',
  channel: 'nightly',
  upToDate: false,
  running: true,
  healthy: true,
  port: 8081,
  staged: null,
  appPath: null,
  busy: LEGACY_BUSY,
  relay: RELAY_UP,
  pendingRestart: false,
  lastUpdate: T3_LAST_UPDATE,
  canUpdate: true,
  canRestart: true,
};

add('legacy-2x.status.json', {
  command: 'status',
  variant: 'legacy-2x',
  schema: 'legacy-2x-status.schema.json',
  description: 'A machine still running agent 2.1.0. There is no `contract` key at all, and that absence is the whole test: a client that finds none uses the key-sniffing paths it has always used, and offers the agent install action instead of the v3 screens.',
  tags: ['legacy', '2x-compatibility'],
}, {
  ok: true,
  os: 'linux',
  system: LINUX,
  hostname: 'atlas',
  agentVersion: '2.1.0',
  services: [LEGACY_SERVICE],
  busy: LEGACY_BUSY,
  bootTargets: [{ id: 'windows', name: 'Windows 11' }],
  actions: [{ id: 'sunshine-restart', name: 'Restart Sunshine', confirm: null, busyGated: true }],
  autoUpdate: true,
  controller: { hash: '3d7251f8764668f6dd9be0e0dcae3ea2ba4a6ecbf07cd0b21f6f66a35bcae6ee' },
  notes: [],
  t3: legacyBlock(LEGACY_SERVICE),
  pendingRestart: false,
  lastUpdate: T3_LAST_UPDATE,
  connect: RELAY_UP,
});

add('legacy-2x.update-deferred.json', {
  command: 'update',
  variant: 'legacy-2x',
  schema: 'legacy-2x-mutation.schema.json',
  description: 'The whole of what 2.x said about a deferred update: an outcome word and a sentence. No operation id, so a reply that never arrived can only be guessed at; no reason code, so "busy" and "automatic updates are off" render the same. Both are why contract 3 exists.',
  tags: ['legacy'],
}, {
  ok: true,
  action: 'deferred',
  service: 't3',
  from: '0.0.36-nightly.20260904',
  to: null,
  message: 'the machine is busy (1 turn running); pass --force to update anyway',
});

add('legacy-2x.config.json', {
  command: 'config',
  variant: 'legacy-2x',
  schema: 'legacy-2x-config.schema.json',
  description: 'A 2.x machine holding a setup document. It reports the hash and nothing else: no identity, no revision, so a 3.x client records the copy as source "legacy" on its own side and never expects an ordering guarantee from the machine.',
  tags: ['legacy', 'controller'],
}, {
  ok: true,
  controller: CONTROLLER_DOC,
  hash: '3d7251f8764668f6dd9be0e0dcae3ea2ba4a6ecbf07cd0b21f6f66a35bcae6ee',
});

// ---------------------------------------------------------------------------

// Optional configured measurements and the administrative configuration editor.
const metricStatus = status({ timing: { budgetMs: 1000, elapsedMs: 1000, partial: true } });
metricStatus.metrics = [
  { id: 'queue-depth', name: 'Queue depth', value: 0, unit: 'jobs', checkedAt: T(1), error: null },
  { id: 'temperature', name: 'Temperature', value: 43.5, unit: '°C', checkedAt: T(1), error: null },
  { id: 'power', name: 'Power', value: null, unit: 'W', checkedAt: T(1), error: 'The configured probe failed.' },
  { id: 'storage', name: 'Storage', value: null, unit: 'GB', checkedAt: null, error: 'The status deadline left no time to start this probe.' },
];
add('status.metrics.json', {
  command: 'status', variant: 'metrics', schema: 'status.schema.json',
  description: 'Explicit configured probes show measured zero, a finite reading, an attempted failure and a probe skipped at the shared deadline; unavailable values remain null.',
  tags: ['configured-telemetry', 'bounded-status'],
}, metricStatus);

const serviceDocument = { configVersion: 3, services: [], updates: { automatic: false }, boot: { targets: {} } };
const configuredService = {
  id: 'demo', name: 'Demo', kind: 'command', process: { type: 'none' }, health: { type: 'none' },
  busy: { type: 'none' }, updates: { automatic: false },
  update: ['node', '/opt/demo/update.mjs'], verify: ['node', '/opt/demo/verify.mjs'],
};
const serviceProposal = { ...serviceDocument, services: [configuredService] };
const structuralBytes = (value) => Array.isArray(value) ? `[${value.map(structuralBytes).join(',')}]`
  : value && typeof value === 'object' ? `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${structuralBytes(value[key])}`).join(',')}}`
  : JSON.stringify(value);
const structuralHash = (value) => crypto.createHash('sha256').update(structuralBytes(value)).digest('hex');
const serviceHash = structuralHash(serviceDocument);
const proposedServiceHash = structuralHash(serviceProposal);
const templateCommon = { id: '', name: '', health: { type: 'none' }, updates: { automatic: false } };
const serviceTemplateRows = [
  { id: 'command', name: 'Command service', description: 'Supply version, update and verification commands. Choose a busy probe, or explicitly declare busy.type none.',
    service: { ...templateCommon, kind: 'command', installedVersion: [], latestVersion: [], update: [], verify: [], process: { type: 'none' } } },
  { id: 'systemd-user', name: 'Systemd user service', description: 'Reference an existing user unit and supply its update and verification commands. Choose a busy probe or explicitly declare none.',
    service: { ...templateCommon, kind: 'command', update: [], verify: [], process: { type: 'systemd-user', unit: '' } } },
  { id: 'npm', name: 'npm service', description: 'Choose a package and its process adapter. Supply a busy probe or explicitly declare none. No package is installed when saving.',
    service: { ...templateCommon, kind: 'npm', package: '', channel: 'latest', allowScripts: [], process: { type: 'none' } } },
];
const serviceChanges = { added: ['demo'], removed: [], changed: [], otherSettingsChanged: false };
const serviceValidation = { ok: true, valid: true, hash: serviceHash, proposedHash: proposedServiceHash, changes: serviceChanges, warnings: [] };
for (const [variant, command, payload, description] of [
  ['get', 'service-config get', { ok: true, hash: serviceHash, document: serviceDocument, templates: serviceTemplateRows, limits: { maxBytes: 1048576 } },
    'An unrestricted administrator reads the full machine-local document, its opaque structural hash, incomplete service templates and the input byte limit.'],
  ['validate', 'service-config validate', serviceValidation,
    'The proposed document validates against the current hash. The reply previews service changes without echoing commands or saving anything.'],
  ['set', 'service-config set', { ...serviceValidation, hash: proposedServiceHash, saved: true, message: 'Agent configuration saved. No setup or service command was executed.' },
    'Saving the exact validated proposal succeeds under compare-and-swap and reports its resulting hash; no service command or package installation runs.'],
  ['conflict', 'service-config set', { ok: false, reasonCode: 'stale-revision', message: 'Agent configuration changed; reload and review your edits before saving.', valid: false, conflict: true, hash: proposedServiceHash },
    'Another administrator changed machine-local configuration after the edit began. The old expectedHash is refused and no proposal is saved.'],
  ['invalid', 'service-config validate', { ok: false, reasonCode: 'config-invalid', message: 'The configuration is incomplete or invalid; nothing was saved.', valid: false,
    errors: [{ path: 'services[0].busy', message: 'Choose a busy probe, or explicitly set type none to allow work without busy protection.' }] },
    'An incomplete service template has no explicit busy decision. Validation refuses it with a field path and safe message, without echoing argv values.'],
  ['restricted', 'service-config get', { ok: false, reasonCode: 'restricted', message: 'Service provisioning requires an unrestricted administrative session.' },
    'A restricted session cannot read, validate or change the machine-local configuration, whose command arguments may contain private values.'],
]) {
  add(`service-config.${variant}.json`, { command, variant, schema: 'service-config.schema.json', description, tags: ['service-config-admin'] }, { ...envelope(), ...payload });
}

add('service-config.profiles.json', {
  command: 'service-config get', variant: 'profiles', schema: 'service-config.schema.json',
  description: 'Detected software profiles distinguish a supported npm install, a desktop application requiring its built-in updater, and an ambiguous product name that cannot be provisioned safely.',
  tags: ['service-config-admin', 'application-profiles'],
}, {
  ...envelope({ system: MAC }), ok: true, hash: serviceHash, document: serviceDocument,
  templates: serviceTemplateRows, limits: { maxBytes: 1048576 },
  profiles: [
    { id: 'codex-cli', name: 'Codex CLI', platform: 'mac', detected: true, availability: 'available',
      message: 'A matching npm installation was detected. Review the draft before saving; automatic maintenance is off and every related process must exit before an update.',
      updateMethod: 'npm', service: {
        id: 'codex-cli', name: 'Codex CLI', kind: 'npm', package: '@openai/codex', npmPrefix: '/opt/homebrew',
        channel: 'latest', allowScripts: [], process: { type: 'none' }, health: { type: 'none' },
        busy: { type: 'command', command: ['/opt/homebrew/bin/node', '/example/base/agent/src/service-profile-probe.mjs', 'busy', 'codex-cli'], timeoutSeconds: 6 },
        updates: { automatic: false },
      } },
    { id: 'codex-desktop', name: 'Codex / ChatGPT desktop', platform: 'mac', detected: true, availability: 'manual',
      message: 'The application is installed. Its built-in updater has no verified unattended command for this install; update through the application.',
      updateMethod: 'application-updater', service: null },
    { id: 'grok-cli', name: 'GrokCLI', platform: 'mac', detected: false, availability: 'unavailable',
      message: 'Several unrelated tools use this name. Select the exact project and distribution before provisioning an updater.',
      updateMethod: null, service: null },
  ],
});

const index = {
  $comment: 'Generated by node contract/tools/make-fixtures.mjs. Edit that file, not these.',
  contract: 3,
  agentVersion: AGENT,
  naming: '<command>.<variant>.json, lowercase and dot-separated. Replies from an agent 2.x are prefixed legacy-2x. and validate against a legacy-2x-*.schema.json.',
  loading: 'Load index.json first and walk `fixtures`; do not glob the directory, because index.json is itself a .json file in it. Every entry names the schema its payload validates against, relative to contract/schemas.',
  hashVectors: '../hash-vectors.json, validated by contract/validate.mjs against schemas/hash-vectors.schema.json. It is not a reply fixture and is deliberately not in this directory.',
  fields: {
    file: 'the fixture, in this directory',
    command: 'the legionctl command whose reply this is',
    variant: 'which answer to that command',
    schema: 'the schema in contract/schemas that this payload validates against',
    ok: 'the reply’s own ok',
    exitCode: 'the process exit code that goes with it: 0 when the command did what was asked, 1 otherwise',
    contract: '3, or null for a reply from an agent 2.x that carries no contract key',
    action: 'the reply’s action, when it has one',
    reasonCode: 'the reply’s reasonCode, when it has one',
    tags: 'what this fixture is for, so a client test can pick a subset',
  },
  fixtures,
  coverageExemptions: { reasonCodes: {} },
};

fs.mkdirSync(OUT, { recursive: true });
fs.writeFileSync(path.join(OUT, 'index.json'), `${JSON.stringify(index, null, 2)}\n`, 'utf8');
process.stdout.write(`wrote ${fixtures.length} fixtures and index.json\n`);
