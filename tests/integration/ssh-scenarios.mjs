#!/usr/bin/env node
// Opt-in end-to-end checks against a separately installed, isolated SSH test target.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import assert from 'node:assert/strict';

const [host, node, base, output] = process.argv.slice(2);
if (!host || !node?.startsWith('/') || !base?.endsWith('/legion-control-test') || !output) {
  throw new Error('Usage: ssh-scenarios.mjs SSH_ALIAS ABSOLUTE_NODE ABSOLUTE_legion-control-test OUTPUT_JSON');
}
const quote = value => `'${String(value).replaceAll("'", "'\\''")}'`;
const data = `${base}/data`;
const agentPath = `${data}/agent/src/index.mjs`;
const fixturePath = `${base}/fixture/service.mjs`;
const fixtureData = `${base}/fixture/data`;
const evidence = [];
function ssh(argv, input = '', budget = 45000) {
  return new Promise((resolve, reject) => {
    const child = spawn('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', host, argv.map(quote).join(' ')], { stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    const timer = setTimeout(() => { child.kill('SIGTERM'); reject(new Error(`SSH test timed out: ${argv.at(-1)}`)); }, budget);
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.on('close', code => { clearTimeout(timer); resolve({ code, stdout, stderr }); });
    child.stdin.end(input);
  });
}
async function agent(args, input = '') {
  const start = Date.now();
  const result = await ssh(['env', `LEGIONCTL_HOME=${data}`, node, agentPath, ...args], input);
  let reply;
  try { reply = JSON.parse(result.stdout.trim()); }
  catch { throw new Error(`Invalid agent JSON for ${args.join(' ')}: ${result.stdout} ${result.stderr}`); }
  evidence.push({ command: args, elapsedMs: Date.now() - start, exitCode: result.code, reply, stderr: result.stderr });
  return reply;
}
async function writeFile(file, contents) {
  assert.ok(file.startsWith(base + '/'));
  const result = await ssh([node, '-e', 'const fs=require("node:fs");let s="";process.stdin.setEncoding("utf8");process.stdin.on("data",x=>s+=x);process.stdin.on("end",()=>fs.writeFileSync(process.argv[1],s));', file], contents);
  assert.equal(result.code, 0, result.stderr);
}
async function fixture(verb, value) {
  const result = await ssh([node, fixturePath, verb, fixtureData, ...(value ? [value] : [])]);
  assert.equal(result.code, 0, result.stderr);
  return result.stdout.trim() ? JSON.parse(result.stdout) : null;
}
const id = () => crypto.randomUUID();
const action = reply => reply.action ?? reply.op?.result?.action;
async function settle(operationId) {
  for (let attempt = 0; attempt < 12; attempt++) {
    const reply = await agent(['op', operationId, '--wait', '10']);
    if (reply.op?.state === 'finished') return reply.op;
  }
  throw new Error(`Operation ${operationId} did not finish.`);
}
const original = await ssh([node, '-e', 'process.stdout.write(require("node:fs").readFileSync(process.argv[1],"utf8"))', `${data}/config.json`]);
assert.equal(original.code, 0, original.stderr);
const config = JSON.parse(original.stdout);
assert.equal(config.system.id, 'pi', 'Only the isolated Pi configuration is accepted.');
assert.equal(config.services[0].id, 'demo');
let passed = 0;
async function scenario(name, run) {
  await run();
  passed++;
  console.log(`PASS ${name}`);
}
try {
  await fixture('set-busy', 'off');
  await writeFile(`${fixtureData}/version`, '1.0.0');
  await writeFile(`${fixtureData}/latest`, '2.0.0');
  await scenario('bounded status reports the isolated service', async () => {
    const result = await agent(['status', '--budget-ms', '5000']);
    assert.equal(result.contract, 3);
    assert.ok(result.services.some(service => service.id === 'demo'));
    assert.ok(evidence.at(-1).elapsedMs < 10000);
  });
  await scenario('manual update works with automatic maintenance disabled and survives session end', async () => {
    await agent(['auto-update', 'off']);
    const operationId = id();
    const result = await agent(['update', '--service', 'demo', '--op', operationId, '--detach']);
    assert.ok(['accepted', 'updated', 'noop'].includes(action(result)), JSON.stringify(result));
    const operation = await settle(operationId);
    assert.equal(operation.result.ok, true, JSON.stringify(operation));
    assert.equal((await fixture('info')).version, '2.0.0');
  });
  await scenario('idempotent actions run once and reject a different intent', async () => {
    const before = (await fixture('info')).actions;
    const operationId = id();
    await agent(['run', 'count', '--op', operationId]);
    await agent(['run', 'count', '--op', operationId]);
    assert.equal((await fixture('info')).actions, before + 1);
    const conflict = await agent(['restart', '--service', 'demo', '--op', operationId]);
    assert.ok(conflict.ok === false || action(conflict) === 'conflict', JSON.stringify(conflict));
  });
  await scenario('busy work is deferred, queued work is cancellable', async () => {
    await fixture('set-busy', 'on');
    const deferred = await agent(['restart', '--service', 'demo', '--op', id()]);
    assert.ok(['busy', 'busy-unknown'].includes(deferred.reasonCode ?? deferred.op?.result?.reasonCode), JSON.stringify(deferred));
    const operationId = id();
    const queued = await agent(['restart', '--service', 'demo', '--op', operationId, '--when-idle', '--expires', '30m']);
    assert.equal(action(queued), 'queued');
    const cancelled = await agent(['cancel', operationId]);
    assert.ok(action(cancelled) === 'cancelled' || cancelled.op?.result?.reasonCode === 'cancelled', JSON.stringify(cancelled));
    await fixture('set-busy', 'off');
  });
  await scenario('queued manual work runs even with automatic maintenance off', async () => {
    await fixture('set-busy', 'on');
    const before = (await fixture('info')).actions;
    const operationId = id();
    await agent(['run', 'count', '--op', operationId, '--when-idle']);
    await fixture('set-busy', 'off');
    await agent(['cycle']);
    const operation = await settle(operationId);
    assert.equal(operation.result.ok, true, JSON.stringify(operation));
    assert.equal((await fixture('info')).actions, before + 1);
  });
  await scenario('corrupt configuration blocks mutations without falling back to defaults', async () => {
    await writeFile(`${data}/config.json`, '{broken');
    const result = await agent(['run', 'count', '--op', id()]);
    assert.equal(result.ok, false);
    assert.equal(result.reasonCode, 'config-invalid');
    await writeFile(`${data}/config.json`, JSON.stringify(config));
  });
  await scenario('a successful command that did not install the target is reported as failure', async () => {
    const failedConfig = structuredClone(config);
    failedConfig.services[0].update = [node, fixturePath, 'noop-update', fixtureData];
    await writeFile(fixtureData + '/previous', '2.0.0');
    await writeFile(fixtureData + '/latest', '3.0.0');
    await writeFile(data + '/config.json', JSON.stringify(failedConfig));
    const result = await agent(['update', '--service', 'demo', '--op', id()]);
    assert.equal(result.ok, false, JSON.stringify(result));
    assert.ok(['postcondition-failed', 'rolled-back'].includes(result.reasonCode), JSON.stringify(result));
    assert.equal((await fixture('info')).version, '2.0.0');
    await fixture('health');
    await writeFile(data + '/config.json', JSON.stringify(config));
    await writeFile(fixtureData + '/latest', '2.0.0');
  });
  await scenario('slow probes return bounded partial status with unknown busy state', async () => {
    const slowConfig = structuredClone(config);
    slowConfig.services[0].busy = { type: 'command', command: [node, fixturePath, 'slow-busy', fixtureData, '16000'] };
    slowConfig.services.push({ ...structuredClone(slowConfig.services[0]), id: 'second', name: 'Second slow probe' });
    await writeFile(data + '/config.json', JSON.stringify(slowConfig));
    const started = Date.now();
    const result = await agent(['status', '--budget-ms', '1200']);
    assert.ok(Date.now() - started < 5000, 'Status must honor its total budget.');
    assert.equal(result.timing.partial, true, JSON.stringify(result));
    assert.ok(result.services.every(service => service.busy.unknown), JSON.stringify(result));
    await writeFile(data + '/config.json', JSON.stringify(config));
  });
  await scenario('native wake helper sends the exact magic packet', async () => {
    const before = (await fixture('info')).wolPackets;
    const result = await agent(['run', 'wake-fixture', '--op', id()]);
    assert.equal(result.ok, true, JSON.stringify(result));
    const actual = await fixture('info');
    const mac = Buffer.from('001122334455', 'hex');
    const expected = Buffer.concat([Buffer.alloc(6, 255), ...Array.from({length: 16}, () => mac)]);
    assert.equal(actual.wolPackets - before, 2);
    assert.equal(actual.lastWol.size, 102);
    assert.equal(actual.lastWol.sha256, crypto.createHash('sha256').update(expected).digest('hex'));
  });
  await scenario('peer setup detects divergent offline edits even when their revision is higher', async () => {
    const setupId = 'setup-' + id();
    const make = (revision, lineage, name, siteName = 'Test site') => ({
      version: 1,
      controller: { id: setupId, name: 'Integration setup', revision, lineage, source: 'cli', device: 'Integration controller', updatedAt: new Date().toISOString() },
      sites: [{ id: 'test-site', name: siteName, lanPrefixes: ['192.168.178.'], broadcast: ['192.168.178.255'] }],
      machines: [{
        id: 'pi', name, site: 'test-site', alwaysOn: true,
        endpoints: [{ id: 'pi-ssh', kind: 'remote', host: '192.168.178.56', port: 22, user: 'x1f4r', system: 'pi' }],
        systems: [{ id: 'pi', name: 'Pi test system', platform: 'linux', agent: [node, agentPath] }]
      }]
    });
    const bytes = document => JSON.stringify(document, null, 2) + '\n';
    const hash = document => crypto.createHash('sha256').update(bytes(document)).digest('hex');
    const push = (document, replace = false) => agent(['config', 'set', '--controller-id', document.controller.id, '--revision', String(document.controller.revision), ...(replace ? ['--replace'] : [])], bytes(document));
    const baseDocument = make(1, [], 'Original Pi');
    assert.equal((await push(baseDocument, true)).ok, true);
    const baseHash = hash(baseDocument);
    const branchA = make(2, [baseHash], 'Renamed by A');
    const branchB = make(2, [baseHash], 'Original Pi', 'Site edited by B');
    assert.equal((await push(branchA)).ok, true);
    const divergent = await push(branchB);
    assert.equal(divergent.reasonCode, 'controller-conflict');
    assert.equal(divergent.divergent, true);
    const advancedA = make(3, [hash(branchA), baseHash], 'Renamed again by A');
    assert.equal((await push(advancedA)).ok, true);
    const advancedB = make(4, [hash(branchB), baseHash], 'Original Pi', 'Site edited again by B');
    assert.equal((await push(advancedB)).reasonCode, 'controller-conflict', 'A higher revision alone must never overwrite another branch.');
    const merged = make(5, [hash(advancedA), hash(advancedB), hash(branchA), hash(branchB), baseHash], 'Renamed again by A', 'Site edited again by B');
    assert.equal((await push(merged)).ok, true);
    const replay = await push(merged);
    assert.equal(replay.ok, true);
    assert.equal(replay.action, 'noop');
    assert.equal((await push(branchA)).reasonCode, 'stale-revision');
    const stored = await agent(['config']);
    assert.equal(stored.hash, hash(merged));
    assert.equal(stored.controller.machines[0].name, 'Renamed again by A');
    assert.equal(stored.controller.sites[0].name, 'Site edited again by B');
    assert.deepEqual(stored.meta.lineage, merged.controller.lineage);
  });
  await scenario('doctor, history and diagnostics are available', async () => {
    const doctor = await agent(['doctor']);
    assert.ok(Array.isArray(doctor.checks));
    const history = await agent(['history', '--limit', '30']);
    assert.ok(Array.isArray(history.operations ?? history.history));
    const bundle = await agent(['bundle']);
    assert.ok(bundle.doctor || bundle.bundle?.doctor);
  });
} finally {
  await writeFile(`${data}/config.json`, original.stdout);
  await fixture('set-busy', 'off');
  fs.mkdirSync(path.dirname(path.resolve(output)), { recursive: true });
  fs.writeFileSync(output, JSON.stringify({ passed, evidence }, null, 2) + '\n');
}
console.log(`${passed} SSH scenarios passed.`);
