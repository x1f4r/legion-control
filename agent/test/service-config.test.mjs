import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { PassThrough } from 'node:stream';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { cli, ENTRY, withHome } from './helpers.mjs';
import { authorize, ownEntry } from '../src/dispatch.mjs';
import { readServiceConfigInput, MAX_SERVICE_CONFIG_BYTES } from '../src/service-config.mjs';

const service = () => ({ id: 'demo', name: 'Demo', kind: 'command', process: { type: 'none' }, health: { type: 'none' }, busy: { type: 'none' }, updates: { automatic: false } });
const get = (home) => cli(['service-config', 'get'], { home }).payload;
const send = (home, verb, request) => cli(['service-config', verb, '--stdin'], { home, stdin: JSON.stringify(request) });

function parallelSet(home, request) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [ENTRY, 'service-config', 'set', '--stdin'], { env: { ...process.env, LEGIONCTL_HOME: home }, stdio: ['pipe', 'pipe', 'pipe'] });
    let output = ''; child.stdout.on('data', (chunk) => { output += chunk; });
    child.on('error', reject); child.on('close', () => { try { resolve(JSON.parse(output)); } catch (error) { reject(error); } });
    child.stdin.end(JSON.stringify(request));
  });
}

test('admin get and validation are read-only, and a reviewed save persists without running commands', async () => withHome(async (home) => {
  const initial = get(home);
  assert.equal(initial.ok, true);
  assert.deepEqual(fs.readdirSync(home), []);
  const marker = path.join(home, 'never-executed');
  const document = { ...initial.document, services: [{ ...service(), update: [process.execPath, '-e', `require('fs').writeFileSync(${JSON.stringify(marker)},'unsafe')`], verify: [process.execPath, '-e', 'process.exit(0)'] }] };
  const request = { expectedHash: initial.hash, document };
  const preview = send(home, 'validate', request);
  assert.equal(preview.payload.valid, true, preview.stdout);
  assert.deepEqual(preview.payload.changes.added, ['demo']);
  assert.deepEqual(fs.readdirSync(home), []);
  const saved = send(home, 'set', request);
  assert.equal(saved.payload.saved, true, saved.stdout);
  assert.equal(saved.payload.hash, preview.payload.proposedHash);
  assert.equal(fs.existsSync(marker), false);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(home, 'config.json'))), document);
  assert.equal(get(home).hash, saved.payload.hash);
}));

test('missing hashes, unknown request fields and stale edits never overwrite config', async () => withHome(async (home) => {
  const initial = get(home);
  const document = { ...initial.document, services: [service()] };
  assert.equal(send(home, 'set', { document }).payload.ok, false);
  assert.equal(send(home, 'validate', { expectedHash: initial.hash, document, surprise: true }).payload.ok, false);
  assert.equal(send(home, 'set', { expectedHash: initial.hash, document }).payload.ok, true);
  const stale = send(home, 'set', { expectedHash: initial.hash, document: initial.document });
  assert.equal(stale.payload.reasonCode, 'stale-revision');
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(home, 'config.json'))).services, [service()]);
}));

test('unversioned configuration gets a reviewable v3 draft while preserving stored CAS identity', async () => withHome(async (home) => {
  const stored = { services: [service()], updates: { automatic: false } };
  const file = path.join(home, 'config.json'); fs.writeFileSync(file, JSON.stringify(stored));
  const initial = get(home);
  assert.equal(initial.document.configVersion, 3); assert.deepEqual(JSON.parse(fs.readFileSync(file)), stored);
  const request = { expectedHash: initial.hash, document: initial.document };
  assert.equal(send(home, 'validate', request).payload.valid, true);
  assert.deepEqual(JSON.parse(fs.readFileSync(file)), stored);
  assert.equal(send(home, 'set', request).payload.saved, true);
  assert.equal(JSON.parse(fs.readFileSync(file)).configVersion, 3);
  fs.writeFileSync(file, JSON.stringify({ ...stored, configVersion: 2 }));
  const explicit = get(home); assert.equal(explicit.document.configVersion, 2);
  assert.equal(send(home, 'validate', { expectedHash: explicit.hash, document: explicit.document }).payload.valid, false);
}));

test('concurrent editor saves cannot both succeed against one config identity', async () => withHome(async (home) => {
  const initial = get(home);
  const replies = await Promise.all(['first', 'second'].map((id) => parallelSet(home, { expectedHash: initial.hash, document: { ...initial.document, services: [{ ...service(), id }] } })));
  assert.equal(replies.filter((reply) => reply.saved).length, 1);
  assert.equal(replies.filter((reply) => !reply.ok).length, 1);
  assert.equal(get(home).document.services.length, 1);
}));

test('save never interrupts a live operation', async () => withHome(async (home) => {
  const initial = get(home);
  const db = new DatabaseSync(path.join(home, 'op.lock.sqlite')); db.exec('BEGIN IMMEDIATE');
  try {
    const reply = send(home, 'set', { expectedHash: initial.hash, document: initial.document });
    assert.equal(reply.payload.reasonCode, 'operation-in-progress');
    assert.equal(fs.existsSync(path.join(home, 'config.json')), false);
  } finally { db.exec('ROLLBACK'); db.close(); }
}));

test('all templates require completion and an explicit busy decision', async () => withHome(async (home) => {
  const initial = get(home);
  assert.deepEqual(initial.templates.map((template) => template.id), ['command', 'systemd-user', 'npm']);
  for (const template of initial.templates) {
    const reply = send(home, 'validate', { expectedHash: initial.hash, document: { ...initial.document, services: [template.service] } });
    assert.equal(reply.payload.valid, false);
  }
  const missingBusy = service(); delete missingBusy.busy;
  const reply = send(home, 'set', { expectedHash: initial.hash, document: { ...initial.document, services: [missingBusy] } });
  assert.equal(reply.payload.valid, false);
  assert.ok(reply.payload.errors.some((issue) => issue.path === 'services[0].busy'));
  assert.equal(fs.existsSync(path.join(home, 'config.json')), false);
}));

test('validation failures never echo rejected command secrets', async () => withHome(async (home) => {
  const initial = get(home);
  const secret = 'private-credential-do-not-echo';
  const request = { expectedHash: initial.hash, document: { ...initial.document, services: [{ ...service(), update: [process.execPath, { token: secret }] }] } };
  const reply = send(home, 'validate', request);
  assert.equal(reply.payload.ok, false);
  assert.doesNotMatch(reply.stdout + reply.stderr, new RegExp(secret));
  assert.equal(fs.existsSync(path.join(home, 'legionctl.log')), false);
}));

test('restricted sessions refuse all provisioning verbs before reading stdin or configuration', async () => withHome(async (home) => {
  fs.writeFileSync(path.join(home, 'config.json'), '{"secret":"do-not-return"}');
  for (const verb of ['get', 'validate', 'set']) {
    const reply = cli(['service-config', verb, ...(verb === 'get' ? [] : ['--stdin'])], { home, env: { LEGIONCTL_RESTRICTED: '1' } });
    assert.equal(reply.payload.reasonCode, 'restricted');
    assert.doesNotMatch(reply.stdout + reply.stderr, /do-not-return/);
    const quoted = (value) => `'${value.replace(/'/g, "'\\''")}'`;
    const authorized = authorize([process.execPath, ownEntry(), 'service-config', verb].map(quoted).join(' '));
    assert.equal(authorized.ok, false);
    assert.equal(authorized.reasonCode, 'restricted');
  }
  assert.deepEqual(fs.readdirSync(home), ['config.json']);
}));

test('service-config CLI accepts only explicit get/validate/set input shapes', async () => withHome(async (home) => {
  for (const args of [[], ['validate'], ['set'], ['get', '--stdin'], ['get', 'extra'], ['set', '--stdin', '--force']]) {
    const reply = cli(['service-config', ...args], { home });
    assert.equal(reply.payload.reasonCode, 'bad-argument');
  }
}));

test('administrative stdin is bounded and timed out without retaining the stream', async () => {
  const hanging = new PassThrough();
  const pending = readServiceConfigInput(hanging, { idleMs: 20, totalMs: 50 });
  const timedOut = await pending;
  assert.equal(timedOut.reasonCode, 'timed-out'); assert.equal(hanging.destroyed, true);
  const oversized = new PassThrough();
  const reading = readServiceConfigInput(oversized);
  oversized.write(Buffer.alloc(MAX_SERVICE_CONFIG_BYTES + 1));
  assert.equal((await reading).reasonCode, 'bad-argument'); assert.equal(oversized.destroyed, true);
  const malformed = new PassThrough(); const parsing = readServiceConfigInput(malformed); malformed.end(Buffer.from([0xff]));
  assert.equal((await parsing).reasonCode, 'bad-argument');
});
