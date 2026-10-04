// Documents and facts shared by the fixtures and the vectors.
//
// A fixture that reports the hash of a setup and a vector that offers a
// descendant of it have to be talking about the same bytes. The only way to
// keep that true is for neither generator to carry its own copy, so the setup,
// its identity and the afternoon every record is timestamped in live here and
// both import them.
//
// Nothing in this file reads a clock, the environment or a random source: the
// generators that use it must produce the same bytes on every machine and on
// every run, or `generate.mjs --check` stops meaning anything.

import { canonicalHash } from './canonical.mjs';

/**
 * One afternoon, in order: an offset in minutes (and optionally seconds and
 * milliseconds) from 2026-09-05T14:00:00.000Z, as the ISO string the agent
 * writes. Negative offsets are the past. A fixture set with four unrelated
 * clocks in it reads as noise, so every timestamp comes from this one.
 */
export function at(minutes, seconds = 0, millis = 0) {
  const base = Date.UTC(2026, 8, 5, 14, 0, 0, 0);
  return new Date(base + minutes * 60000 + seconds * 1000 + millis).toISOString();
}

/** The text every generator writes for a JSON value: two-space indentation, LF, one final newline. */
export function documentText(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

/**
 * Freeze a value all the way down. The shared documents are spread into dozens
 * of fixtures; a generator that edited one in place would quietly change every
 * fixture built after it, and this turns that into an error at the line that
 * did it.
 */
function frozen(value) {
  if (value !== null && typeof value === 'object') {
    for (const child of Object.values(value)) frozen(child);
    Object.freeze(value);
  }
  return value;
}

// ---------------------------------------------------------------------------
// The shared setup and the chain of documents it has been through.
//
// The two ancestor hashes and the other branch are stand-ins: no fixture needs
// their documents, only the fact that they are distinct sha256 values. The
// current hash is not a stand-in. It is COMPUTED from the exact bytes written to
// fixtures/controller-document.sites-helpers.json, so a client that
// canonicalises that file with its own implementation lands on the number the
// config replies report, and a hand-typed constant would have proved nothing.
// ---------------------------------------------------------------------------

export const SETUP_ID = 'setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846';
export const OTHER_SETUP_ID = 'setup-8b04d193-6c27-4a51-83ef-90b7d2c46e15';
export const H_PARENT = '1c680fab2ecc79f4b1c2a06e4e3ed2f8a1d54c6f0b93e7a25c8d0f416b2e93a7';
export const H_GRANDPARENT = '1a5ee25ee34b2f9c07d61a8b45e0c3f27d9b8140a6532e7fc019d84b6a3e5027';
export const H_OTHER_BRANCH = '4a88b8efec1592c0d3b7615ae820f4c96d1e037b58a2c94f6013e7d58b2a06cf';
export const LINEAGE = frozen([H_PARENT, H_GRANDPARENT]);

// Two of the sites carry the SAME private subnet on purpose. That is what two
// homes behind stock routers look like, and it is the case a prefix match
// cannot decide.
export const CONTROLLER_DOC = frozen({
  version: 1,
  controller: {
    id: SETUP_ID,
    name: 'Home setup',
    revision: 12,
    updatedAt: at(-4200),
    source: 'mac',
    device: 'Office MacBook Pro',
    lineage: LINEAGE,
  },
  sites: [
    { id: 'attic', name: 'Attic house', lanPrefixes: ['192.168.50.'], broadcast: ['192.168.50.255'] },
    { id: 'flat', name: 'New flat', lanPrefixes: ['192.168.50.'], broadcast: ['192.168.50.255'] },
  ],
  machines: [
    {
      id: 'pi',
      name: 'Beacon',
      site: 'attic',
      alwaysOn: true,
      endpoints: [
        { id: 'lan', kind: 'lan', host: '192.168.50.40', user: 'me', port: 22 },
        { id: 'remote', kind: 'remote', host: 'beacon.example-tailnet.ts.net', user: 'me', port: 22 },
      ],
      systems: [
        { id: 'linux', name: 'Raspberry Pi OS', platform: 'linux', shell: 'posix', restricted: true, agent: ['node', '/home/me/.legion-control/agent/src/index.mjs'] },
      ],
    },
    {
      id: 'legion',
      name: 'Legion 7',
      site: 'attic',
      alwaysOn: false,
      endpoints: [
        { id: 'lan-linux', kind: 'lan', host: '192.168.50.41', user: 'me', port: 22, system: 'linux' },
        { id: 'remote-linux', kind: 'remote', host: 'legion-linux.example-tailnet.ts.net', user: 'me', port: 22, system: 'linux' },
        { id: 'remote-windows', kind: 'remote', host: 'legion-windows.example-tailnet.ts.net', user: 'me', port: 22, system: 'windows' },
      ],
      wake: {
        mac: 'AA:BB:CC:DD:EE:FF',
        broadcast: ['192.168.50.255'],
        ports: [9, 7],
        lanPrefix: '192.168.50.',
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
        { id: 'linux', name: 'CachyOS', platform: 'linux', shell: 'posix', agent: ['node', '/home/me/.legion-control/agent/src/index.mjs'] },
        { id: 'windows', name: 'Windows 11', platform: 'windows', shell: 'powershell', agent: ['node', 'C:/Users/me/.legion-control/agent/src/index.mjs'] },
      ],
    },
    {
      id: 'nas',
      name: 'Attic NAS',
      site: 'attic',
      alwaysOn: true,
      endpoints: [{ id: 'lan', kind: 'lan', host: '192.168.50.10', user: 'me', port: 22 }],
      systems: [
        { id: 'linux', name: 'Debian', platform: 'linux', shell: 'posix', agent: ['node', '/home/me/.legion-control/agent/src/index.mjs'] },
      ],
    },
    {
      id: 'tower',
      name: 'Tower',
      site: 'flat',
      alwaysOn: false,
      endpoints: [
        { id: 'lan', kind: 'lan', host: '192.168.50.20', user: 'me', port: 22 },
        { id: 'remote', kind: 'remote', host: 'tower.example-tailnet.ts.net', user: 'me', port: 22 },
      ],
      wake: {
        mac: '11:22:33:44:55:66',
        broadcast: ['192.168.50.255'],
      },
      systems: [
        { id: 'linux', name: 'CachyOS', platform: 'linux', shell: 'posix', agent: ['node', '/home/me/.legion-control/agent/src/index.mjs'] },
        { id: 'windows', name: 'Windows 11', platform: 'windows', shell: 'cmd', agent: ['node', 'C:/Users/me/.legion-control/agent/src/index.mjs'] },
      ],
    },
  ],
  appUpdates: { githubRepo: 'x1f4r/legion-control' },
});

/** Exactly the bytes the fixture file holds, which are already canonical: LF only and one final newline. */
export const CONTROLLER_DOC_TEXT = documentText(CONTROLLER_DOC);
export const H_CURRENT = canonicalHash(CONTROLLER_DOC_TEXT);
export const CONTROLLER_DOC_SIZE = Buffer.byteLength(CONTROLLER_DOC_TEXT, 'utf8');

/**
 * A small setup for the cases that are about one rule: one machine called
 * tower, one endpoint and one system. The refused fixtures each break exactly
 * one thing in a copy of it, so the thing they break is the only thing that can
 * be refusing them.
 */
export const TOWER_DOC = frozen({
  version: 1,
  controller: { id: SETUP_ID, revision: 3, lineage: [] },
  machines: [
    {
      id: 'tower',
      name: 'Tower',
      endpoints: [{ id: 'lan', kind: 'lan', host: 'desk-tower.local', user: 'me', port: 22 }],
      systems: [
        { id: 'linux', name: 'Linux', platform: 'linux', shell: 'posix', agent: ['/usr/bin/node', '/home/me/.legion-control/bin/launcher.mjs'] },
      ],
    },
  ],
});

/** A mutable deep copy of a shared document, for a generator that needs to change one thing in it. */
export function copyOf(document) {
  return structuredClone(document);
}
