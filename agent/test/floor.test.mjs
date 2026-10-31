// The version floor (contract section 13).
//
// Every release ever signed stays valid, so before the floor a restricted key
// could pipe an old public release to `self-update --stdin`, or --rollback to
// one, and put back whatever a later release had fixed in its dispatcher. Now
// a restricted key only moves the agent forward, going back takes
// --allow-downgrade from an unrestricted session, and the stable launcher
// refuses a signed tree below the floor. Every refusal comes before an
// operation record and leaves the machine exactly as it was.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { PassThrough } from 'node:stream';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import { checkReply, schemaForCommand } from '../../contract/validate.mjs';
import { compareVersions, judgeAgentTransition } from '../src/floor.mjs';
import { REPO, SRC } from './helpers.mjs';
import { AGENT, bundleTree, isolatedModule, patchLauncherTrust, signingPair, signTree } from './signing.mjs';

const SYSTEM = { system: { id: 'test' } };
const fixture = (name) => JSON.parse(fs.readFileSync(path.join(REPO, 'contract', 'fixtures', name), 'utf8'));

/** A signed tree whose entry answers the self-test as `version`, declaring `floor` when given. */
function signedTree(dir, version, privateKey, { floor = null } = {}) {
  fs.mkdirSync(path.join(dir, 'src'), { recursive: true });
  fs.cpSync(path.join(AGENT, 'install'), path.join(dir, 'install'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'src', 'index.mjs'), `console.log(JSON.stringify({ok:true,selfTest:{ok:true},contract:3,agentVersion:${JSON.stringify(version)}}))\n`);
  fs.writeFileSync(path.join(dir, 'package.json'), `${JSON.stringify({ version, ...(floor ? { legionControl: { agentFloor: floor } } : {}) })}\n`);
  signTree(dir, privateKey);
  return dir;
}

// The fixtures are written from agent/package.json, so a reply held to one comes
// from a live tree of that version.
const PACKAGE_VERSION = JSON.parse(fs.readFileSync(path.join(AGENT, 'package.json'), 'utf8')).version;
const versionOf = (dir) => JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')).version;
const floorFile = (base) => path.join(base, 'agent-floor.json');
const readFloor = (base) => JSON.parse(fs.readFileSync(floorFile(base), 'utf8'));
const writeFloor = (base, version) => fs.writeFileSync(floorFile(base), `${JSON.stringify({ schema: 1, version, updatedAt: '2026-01-01T00:00:00.000Z', reason: 'install' })}\n`);

/**
 * An isolated agent trusting a key of its own, with a home whose live tree is
 * the signed `live` version. The environment is put back afterwards.
 */
async function withAgent(body, { live = '3.1.0', liveFloor = '3.1.0', patch = null } = {}) {
  const isolated = await isolatedModule({ patch });
  const base = path.join(isolated.root, 'home');
  fs.mkdirSync(base);
  if (live) signedTree(path.join(base, 'agent'), live, isolated.pair.privateKey, { floor: liveFloor });
  const saved = { home: process.env.LEGIONCTL_HOME, restricted: process.env.LEGIONCTL_RESTRICTED };
  process.env.LEGIONCTL_HOME = base;
  delete process.env.LEGIONCTL_RESTRICTED;
  try {
    return await body({ ...isolated, base, candidate: (name, version, options) => signedTree(path.join(isolated.root, name), version, isolated.pair.privateKey, options) });
  } finally {
    for (const [name, value] of [['LEGIONCTL_HOME', saved.home], ['LEGIONCTL_RESTRICTED', saved.restricted]]) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
    fs.rmSync(isolated.root, { recursive: true, force: true });
  }
}

const install = (module, tree, options = {}) => module.runSelfUpdate(SYSTEM, { stdin: true, input: PassThrough.from([bundleTree(tree)]), ...options });

/** Nothing was staged, swapped, published or recorded. */
function untouched(base, opId, liveVersion) {
  assert.equal(versionOf(path.join(base, 'agent')), liveVersion);
  for (const left of ['agent.new', 'agent.prev', 'agent.rollback.new', 'bin']) assert.equal(fs.existsSync(path.join(base, left)), false, `${left} exists`);
  assert.equal(fs.existsSync(path.join(base, 'ops', `${opId}.json`)), false, 'a refusal opens no operation record');
}

// ---------------------------------------------------------------------------
// The judge
// ---------------------------------------------------------------------------

test('versions compare by number, and a value that is not a version is never older', () => {
  assert.ok(compareVersions('3.10.0', '3.9.0') > 0);
  assert.ok(compareVersions('3.0.9', '3.1.0') < 0);
  assert.equal(compareVersions('3.1.0', '3.1.0'), 0);
  assert.equal(compareVersions('3.1', '3.1.0'), null);
  assert.equal(judgeAgentTransition({ mode: 'install', target: '3.0.0', live: null, floor: null }).ok, true);
});

test('the judge follows the table of contract section 13', () => {
  const judge = (row, session) => judgeAgentTransition({ ...row, ...session });
  const sessions = { restricted: { restricted: true }, plain: {}, flag: { allowDowngrade: true }, restrictedFlag: { restricted: true, allowDowngrade: true } };
  const rows = [
    // Forward, or a reinstall: allowed everywhere, the flag ignored.
    { row: { mode: 'install', target: '3.2.0', live: '3.1.0', floor: '3.1.0' }, expect: { restricted: 'ok', plain: 'ok', flag: 'ok', lower: false } },
    { row: { mode: 'install', target: '3.1.0', live: '3.1.0', floor: '3.1.0' }, expect: { restricted: 'ok', plain: 'ok', flag: 'ok', lower: false } },
    { row: { mode: 'install', target: '3.2.0', live: '3.1.0', floor: '3.1.0', floorUnreadable: true }, expect: { restricted: 'ok', plain: 'ok', flag: 'ok', lower: false } },
    // Below the floor: the flag lowers it.
    { row: { mode: 'install', target: '3.0.0', live: '3.1.0', floor: '3.1.0' }, expect: { restricted: 'restricted', plain: 'bad-argument', flag: 'ok', lower: true } },
    // Below the live version but not the floor: the flag is needed, the floor stays.
    { row: { mode: 'install', target: '3.1.0', live: '3.2.0', floor: '3.1.0' }, expect: { restricted: 'restricted', plain: 'bad-argument', flag: 'ok', lower: false } },
    // With the floor unreadable, going below the live version rewrites it.
    { row: { mode: 'install', target: '3.1.0', live: '3.2.0', floor: '3.1.0', floorUnreadable: true }, expect: { restricted: 'restricted', plain: 'bad-argument', flag: 'ok', lower: true } },
    // A rollback goes back by design and is held to the floor alone.
    { row: { mode: 'rollback', target: '3.0.0', live: '3.1.0', floor: null }, expect: { restricted: 'ok', plain: 'ok', flag: 'ok', lower: false } },
    { row: { mode: 'rollback', target: '3.1.0', live: '3.2.0', floor: '3.1.0' }, expect: { restricted: 'ok', plain: 'ok', flag: 'ok', lower: false } },
    { row: { mode: 'rollback', target: '3.0.0', live: '3.1.0', floor: '3.1.0' }, expect: { restricted: 'restricted', plain: 'bad-argument', flag: 'ok', lower: true } },
    // A floor nobody can read is not permission to roll back past it.
    { row: { mode: 'rollback', target: '3.2.0', live: '3.3.0', floor: '3.1.0', floorUnreadable: true }, expect: { restricted: 'restricted', plain: 'bad-argument', flag: 'ok', lower: true } },
  ];
  for (const { row, expect } of rows) {
    const where = JSON.stringify(row);
    for (const [name, session] of Object.entries(sessions)) {
      const verdict = judge(row, session);
      const wanted = name === 'restrictedFlag' ? expect.restricted : expect[name];
      assert.equal(verdict.ok ? 'ok' : verdict.reasonCode, wanted, `${where} in a ${name} session`);
      if (verdict.ok) assert.equal(verdict.lowerFloor, name === 'flag' && expect.lower, `${where} in a ${name} session lowers the floor`);
    }
  }
  assert.equal(
    judge(rows[3].row, sessions.restricted).message,
    'agent 3.0.0 is below the version floor 3.1.0; a restricted key can only move the agent forward or roll back to a signed agent at or above 3.1.0',
  );
  assert.match(judge(rows[4].row, sessions.plain).message, /^agent 3\.1\.0 is older than the installed 3\.2\.0; nothing was staged\. Repeat with --allow-downgrade/);
  assert.match(judge(rows[9].row, sessions.restricted).message, /cannot be checked against the version floor because agent-floor\.json cannot be read/);
});

test('a release must declare its floor as x.y.z, at most its own version', async () => {
  await withAgent(async ({ src, candidate }) => {
    const { verifyAgentArchive } = await import(`${pathToFileURL(path.join(src, 'archive.mjs')).href}?floor`);
    const declares = (name, version, floor) => verifyAgentArchive(bundleTree(candidate(name, version, { floor })));
    assert.equal(declares('none', '3.0.0', null).agentFloor, null, 'a release from before the floor declares none and stays installable');
    assert.equal(declares('own', '3.1.0', '3.1.0').agentFloor, '3.1.0');
    assert.equal(declares('older', '3.2.0', '3.1.0').agentFloor, '3.1.0');
    for (const [name, floor] of [['above', '3.2.0'], ['short', '3.1'], ['word', 'latest']]) {
      const refused = declares(name, '3.1.0', floor);
      assert.equal(refused.ok, false, name);
      assert.match(refused.error, /agentFloor .* not an x\.y\.z version at most 3\.1\.0/);
    }
  });
});

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

test('a restricted key cannot install an older signed agent, and nothing is staged or recorded', async () => {
  await withAgent(async ({ module, base, candidate }) => {
    process.env.LEGIONCTL_RESTRICTED = '1';
    const older = candidate('older', '3.0.0');
    const refused = await install(module, older, { opId: 'restricted-older-0001' });
    assert.equal(refused.reasonCode, 'restricted');
    assert.equal(refused.action, 'failed');
    assert.equal(refused.message, fixture('self-update.downgrade-restricted.json').message);
    assert.equal(refused.manifest.version, '3.0.0', 'the signature was checked first, so the refusal names what it refused');
    untouched(base, 'restricted-older-0001', '3.1.0');
    assert.equal(fs.existsSync(floorFile(base)), false);

    // The same key may move the agent forward, and the committed install
    // records the floor the new release declares.
    const newer = await install(module, candidate('newer', '3.2.0', { floor: '3.2.0' }), { opId: 'restricted-newer-0001' });
    assert.equal(newer.action, 'installed', newer.message);
    assert.equal(versionOf(path.join(base, 'agent')), '3.2.0');
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.2.0', 'install']);

    // Below the live version is refused even where it is above the floor.
    const back = await install(module, candidate('back', '3.1.0', { floor: '3.1.0' }), { opId: 'restricted-back-0001' });
    assert.equal(back.reasonCode, 'restricted');
    assert.match(back.message, /below the version floor 3\.2\.0/);
  });
});

test('an unrestricted session needs --allow-downgrade, which lowers the floor before the swap', async () => {
  await withAgent(async ({ module, base, candidate }) => {
    writeFloor(base, '3.1.0');
    const older = candidate('older', '3.0.0');
    const refused = await install(module, older, { opId: 'plain-older-0001' });
    assert.equal(refused.reasonCode, 'bad-argument');
    assert.equal(refused.message, fixture('self-update.downgrade-needs-flag.json').message);
    untouched(base, 'plain-older-0001', '3.1.0');
    assert.equal(readFloor(base).version, '3.1.0');

    const installed = await install(module, older, { opId: 'plain-older-0002', allowDowngrade: true });
    assert.equal(installed.action, 'installed', installed.message);
    assert.equal(versionOf(path.join(base, 'agent')), '3.0.0');
    assert.equal(versionOf(path.join(base, 'agent.prev')), '3.1.0');
    // The outgoing tree declared 3.1.0, but a deliberate downgrade has gone
    // below it, so committing does not raise the floor back above the tree.
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.0.0', 'downgrade']);
    if (process.platform !== 'win32') assert.equal(fs.statSync(floorFile(base)).mode & 0o777, 0o600, 'only this user reads or writes the floor');
  });
});

test('a swap that does not commit puts the floor back, and only a committed install raises it', async () => {
  // The swap fails at the rename that would commit it, after keeping a copy of
  // the floor file as it was at that moment. The statement is matched without
  // its line ending, which a Windows checkout may have turned into CRLF.
  const patch = (src) => {
    const file = path.join(src, 'agent-swap.mjs');
    const text = fs.readFileSync(file, 'utf8');
    const commit = '    io.renameSync(p.staging, p.live);';
    assert.equal(text.split(commit).length, 2);
    fs.writeFileSync(file, text.replace(commit, () => `    if (exists(path.join(base, 'agent-floor.json'), io)) io.writeFileSync(path.join(base, 'floor-at-swap.json'), io.readFileSync(path.join(base, 'agent-floor.json')));\n    throw new Error('injected swap failure');`));
  };
  await withAgent(async ({ module, base, candidate }) => {
    writeFloor(base, '3.1.0');
    const failed = await install(module, candidate('older', '3.0.0'), { opId: 'failed-downgrade-0001', allowDowngrade: true });
    assert.equal(failed.ok, false);
    assert.match(failed.message, /injected swap failure/);
    const atSwap = JSON.parse(fs.readFileSync(path.join(base, 'floor-at-swap.json'), 'utf8'));
    assert.deepEqual([atSwap.version, atSwap.reason], ['3.0.0', 'downgrade'], 'the floor was lowered before the swap');
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.1.0', 'restore']);
    assert.equal(versionOf(path.join(base, 'agent')), '3.1.0');

    fs.rmSync(floorFile(base));
    const missing = await install(module, candidate('older-again', '3.0.0'), { opId: 'failed-downgrade-0002', allowDowngrade: true });
    assert.equal(missing.ok, false);
    assert.equal(fs.existsSync(floorFile(base)), false, 'a floor file that was not there is not left behind');

    const upgrade = await install(module, candidate('newer', '3.2.0', { floor: '3.2.0' }), { opId: 'failed-upgrade-0001' });
    assert.equal(upgrade.ok, false);
    assert.equal(fs.existsSync(floorFile(base)), false, 'an install that did not commit raises nothing');
  }, { patch });
});

test('the judge under the lock sees the agent that an interrupted swap puts back', async () => {
  await withAgent(async ({ module, src, base, candidate }) => {
    writeFloor(base, '3.1.0');
    const floorBefore = fs.readFileSync(floorFile(base), 'utf8');
    // An install of 3.4.0 dies between parking the live 3.3.0 and renaming the
    // new tree into place. With no agent/ at all, a judge before the lock can
    // only go by the running agent's own 3.1.0.
    fs.cpSync(candidate('interrupted', '3.4.0', { floor: '3.1.0' }), path.join(base, 'agent.new'), { recursive: true });
    const crashed = spawnSync(process.execPath, ['--input-type=module', '-e', `
      import fs from 'node:fs';
      import path from 'node:path';
      import { commitInstallSwap } from ${JSON.stringify(pathToFileURL(path.join(src, 'agent-swap.mjs')).href)};
      const base = process.argv[1];
      const io = new Proxy(fs, { get(target, property) {
        if (property !== 'renameSync') return Reflect.get(target, property);
        return (source, destination) => {
          if (source === path.join(base, 'agent.new')) process.exit(77);
          fs.renameSync(source, destination);
        };
      }});
      commitInstallSwap(base, { io });
    `, base], { encoding: 'utf8', timeout: 30_000 });
    assert.equal(crashed.status, 77, crashed.stderr);
    assert.equal(fs.existsSync(path.join(base, 'agent')), false);
    assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent-swap.json'), 'utf8')).phase, 'live-parked');

    process.env.LEGIONCTL_RESTRICTED = '1';
    const refused = await install(module, candidate('older', '3.2.0', { floor: '3.1.0' }), { opId: 'after-interrupted-0001' });
    assert.equal(refused.reasonCode, 'restricted');
    assert.match(refused.message, /^agent 3\.2\.0 is below the installed 3\.3\.0/);
    assert.equal(fs.existsSync(path.join(base, 'ops', 'after-interrupted-0001.json')), true, 'the judge before the lock let it through');
    assert.equal(versionOf(path.join(base, 'agent')), '3.3.0', 'recovery put the parked tree back, and nothing replaced it');
    assert.equal(fs.existsSync(path.join(base, 'agent.prev')), false);
    assert.equal(fs.readFileSync(floorFile(base), 'utf8'), floorBefore);
  }, { live: '3.3.0', liveFloor: '3.1.0' });
});

test('a retried install answers with its first result, whatever has been installed since', async () => {
  await withAgent(async ({ module, base, candidate }) => {
    process.env.LEGIONCTL_RESTRICTED = '1';
    const first = candidate('first', '3.2.0', { floor: '3.2.0' });
    const installed = await install(module, first, { opId: 'retried-install-0001' });
    assert.equal(installed.action, 'installed', installed.message);
    const later = await install(module, candidate('later', '3.3.0', { floor: '3.3.0' }), { opId: 'later-install-0001' });
    assert.equal(later.action, 'installed', later.message);

    // The reply to the first install was lost, and its client sends the same
    // request again. Judged against 3.3.0 it would be refused, and the client
    // would report a failure for an install that succeeded.
    const retried = await install(module, first, { opId: 'retried-install-0001' });
    assert.equal(retried.replayed, true);
    assert.equal(retried.action, 'installed', retried.message);
    assert.equal(retried.to, '3.2.0');
    assert.equal(versionOf(path.join(base, 'agent')), '3.3.0', 'nothing was installed a second time');
    assert.equal(readFloor(base).version, '3.3.0');

    // The same id with another bundle is not a retry, and is judged like any
    // other request.
    const reused = await install(module, candidate('older', '3.0.0'), { opId: 'retried-install-0001' });
    assert.equal(reused.reasonCode, 'restricted');
    assert.equal(versionOf(path.join(base, 'agent')), '3.3.0');
  });
});

// ---------------------------------------------------------------------------
// Rolling back
// ---------------------------------------------------------------------------

test('a rollback is held to the floor: at or above it for a restricted key, below it only deliberately', async () => {
  await withAgent(async ({ module, base, pair }) => {
    signedTree(path.join(base, 'agent.prev'), '3.0.0', pair.privateKey);
    process.env.LEGIONCTL_RESTRICTED = '1';
    const restricted = await module.runRollback(SYSTEM, { opId: 'rollback-restricted-0001' });
    assert.equal(restricted.reasonCode, 'restricted');
    assert.match(restricted.message, /^agent 3\.0\.0 is below the version floor 3\.1\.0/);
    delete process.env.LEGIONCTL_RESTRICTED;
    const plain = await module.runRollback(SYSTEM, { opId: 'rollback-plain-0001' });
    assert.equal(plain.reasonCode, 'bad-argument');
    for (const id of ['rollback-restricted-0001', 'rollback-plain-0001']) assert.equal(fs.existsSync(path.join(base, 'ops', `${id}.json`)), false);
    assert.equal(versionOf(path.join(base, 'agent')), '3.1.0');
    assert.equal(fs.existsSync(floorFile(base)), false);

    const deliberate = await module.runRollback(SYSTEM, { opId: 'rollback-flag-0001', allowDowngrade: true });
    assert.equal(deliberate.action, 'rolled-back', deliberate.message);
    assert.equal(versionOf(path.join(base, 'agent')), '3.0.0');
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.0.0', 'downgrade']);
  });

  await withAgent(async ({ module, base, pair }) => {
    // A retained tree at the floor is a rollback a restricted key may make,
    // and a rollback raises nothing.
    signedTree(path.join(base, 'agent.prev'), '3.1.0', pair.privateKey, { floor: '3.1.0' });
    writeFloor(base, '3.1.0');
    process.env.LEGIONCTL_RESTRICTED = '1';
    const rolled = await module.runRollback(SYSTEM, { opId: 'rollback-at-floor-0001' });
    assert.equal(rolled.action, 'rolled-back', rolled.message);
    assert.equal(versionOf(path.join(base, 'agent')), '3.1.0');
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.1.0', 'install']);
  }, { live: '3.2.0', liveFloor: '3.1.0' });
});

test('an unreadable floor holds back a restricted rollback, never an upgrade, and is not written over', async () => {
  await withAgent(async ({ module, base, pair, candidate }) => {
    signedTree(path.join(base, 'agent.prev'), '3.2.0', pair.privateKey, { floor: '3.1.0' });
    const garbage = '{"schema":1,"version":';
    fs.writeFileSync(floorFile(base), garbage);
    process.env.LEGIONCTL_RESTRICTED = '1';
    const rollback = await module.runRollback(SYSTEM, { opId: 'unreadable-rollback-0001' });
    assert.equal(rollback.reasonCode, 'restricted');
    assert.match(rollback.message, /cannot be checked against the version floor/);

    const upgrade = await install(module, candidate('newer', '3.4.0', { floor: '3.4.0' }), { opId: 'unreadable-upgrade-0001' });
    assert.equal(upgrade.action, 'installed', upgrade.message);
    assert.equal(fs.readFileSync(floorFile(base), 'utf8'), garbage, 'writing over it could lower a floor nobody can see');
    assert.match(upgrade.notes.join(' '), /version floor was not raised/);

    // An unrestricted session that goes back deliberately records a floor again.
    delete process.env.LEGIONCTL_RESTRICTED;
    const deliberate = await module.runRollback(SYSTEM, { opId: 'unreadable-rollback-0002', allowDowngrade: true });
    assert.equal(deliberate.action, 'rolled-back', deliberate.message);
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.3.0', 'downgrade']);
  }, { live: '3.3.0', liveFloor: '3.1.0' });
});

// ---------------------------------------------------------------------------
// The reply on the wire
// ---------------------------------------------------------------------------

test('the refusals a client sees are the ones the fixtures show', async () => {
  await withAgent(async ({ src, base, candidate }) => {
    const bundle = bundleTree(candidate('older', '3.0.0'));
    const run = (args, env = {}) => {
      const result = spawnSync(process.execPath, [path.join(src, 'index.mjs'), 'self-update', ...args], {
        encoding: 'utf8', input: bundle, env: { ...process.env, LEGIONCTL_HOME: base, ...env }, timeout: 60_000,
      });
      assert.equal(result.status, 1, result.stderr);
      const reply = JSON.parse(result.stdout);
      assert.deepEqual(checkReply(schemaForCommand('self-update', reply), reply), []);
      return reply;
    };
    for (const [name, env] of [['self-update.downgrade-restricted.json', { LEGIONCTL_RESTRICTED: '1' }], ['self-update.downgrade-needs-flag.json', {}]]) {
      const reply = run(['--stdin', '--op', 'wire-refusal-0001'], env);
      const shown = fixture(name);
      assert.deepEqual(Object.keys(reply).sort(), Object.keys(shown).sort(), name);
      for (const key of ['ok', 'reasonCode', 'message', 'outcome', 'action', 'current', 'floor', 'staged', 'op']) assert.deepEqual(reply[key], shown[key], `${name}: ${key}`);
    }
    // A restricted session never gets to ask, however it reached the agent.
    const flagged = run(['--stdin', '--allow-downgrade'], { LEGIONCTL_RESTRICTED: '1' });
    assert.equal(flagged.reasonCode, 'restricted');
    assert.equal(flagged.argument, '--allow-downgrade');
    assert.equal(flagged.outcome, 'failed');
    assert.equal(versionOf(path.join(base, 'agent')), PACKAGE_VERSION);
  }, { live: PACKAGE_VERSION });
});

// ---------------------------------------------------------------------------
// The launcher, recovery and promotion
// ---------------------------------------------------------------------------

/** A base with the stable launcher in bin/, trusting a test key. */
function launcherBase() {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'legion-floor-launcher-')));
  const pair = signingPair();
  fs.mkdirSync(path.join(base, 'bin'));
  fs.copyFileSync(path.join(AGENT, 'install', 'launcher.mjs'), path.join(base, 'bin', 'launcher.mjs'));
  patchLauncherTrust(path.join(base, 'bin', 'launcher.mjs'), pair.publicKey);
  const tree = (name, version, label, { signed = true, floor = null } = {}) => {
    const dir = path.join(base, name);
    fs.mkdirSync(path.join(dir, 'src'), { recursive: true });
    fs.writeFileSync(path.join(dir, 'src', 'index.mjs'), `process.stdout.write(${JSON.stringify(`${label}\n`)});\n`);
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({ version, type: 'module', ...(floor ? { legionControl: { agentFloor: floor } } : {}) }));
    if (signed) signTree(dir, pair.privateKey);
    return dir;
  };
  const launch = () => spawnSync(process.execPath, [path.join(base, 'bin', 'launcher.mjs'), 'status'], { encoding: 'utf8', env: { ...process.env, LEGIONCTL_HOME: base } });
  return { base, pair, tree, launch };
}

test('the launcher refuses a signed agent below the recorded floor and only warns about an unreadable one', () => {
  const { base, tree, launch } = launcherBase();
  try {
    tree('agent', '3.0.0', 'old-live');
    writeFloor(base, '3.1.0');
    const refused = launch();
    assert.equal(refused.status, 1);
    assert.equal(refused.stdout, '', 'no agent code ran');
    assert.ok(refused.stderr.includes(`refusing to execute agent 3.0.0: it is below the version floor 3.1.0 recorded in ${floorFile(base)}; install a signed agent at or above 3.1.0`), refused.stderr);

    writeFloor(base, '3.0.0');
    assert.equal(launch().stdout, 'old-live\n');

    fs.writeFileSync(floorFile(base), 'not json');
    const warned = launch();
    assert.equal(warned.stdout, 'old-live\n');
    assert.match(warned.stderr, /version floor was not checked/);

    // An unsigned tree has no floor to be held to: a source checkout runs.
    fs.rmSync(path.join(base, 'agent'), { recursive: true });
    tree('agent', '3.0.0', 'checkout', { signed: false });
    writeFloor(base, '9.0.0');
    assert.equal(launch().stdout, 'checkout\n');
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('recovering an interrupted swap never consults the floor; running the restored tree does', () => {
  const { base, tree, launch } = launcherBase();
  try {
    tree('agent.prev', '3.0.0', 'pre-swap');
    tree('agent.new', '3.1.0', 'candidate');
    fs.writeFileSync(path.join(base, 'agent-swap.json'), JSON.stringify({ schema: 1, operation: 'install', phase: 'live-parked' }));
    writeFloor(base, '3.1.0');
    const result = launch();
    assert.match(result.stderr, /restored the pre-swap agent/);
    assert.equal(versionOf(path.join(base, 'agent')), '3.0.0', 'the tree that was live when the swap began is back');
    assert.equal(result.status, 1);
    assert.match(result.stderr, /below the version floor 3\.1\.0/);

    fs.writeFileSync(floorFile(base), 'not json');
    assert.equal(launch().stdout, 'pre-swap\n');
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('the installer promotes a signed agent only at or above the recorded floor, and then raises it', () => {
  const { base, pair, tree } = launcherBase();
  try {
    const source = path.join(base, 'installer');
    fs.mkdirSync(path.join(source, 'install'), { recursive: true });
    fs.mkdirSync(path.join(source, 'src'));
    fs.copyFileSync(path.join(AGENT, 'install', 'promote.mjs'), path.join(source, 'install', 'promote.mjs'));
    fs.copyFileSync(path.join(AGENT, 'install', 'launcher.mjs'), path.join(source, 'install', 'launcher.mjs'));
    patchLauncherTrust(path.join(source, 'install', 'launcher.mjs'), pair.publicKey);
    fs.copyFileSync(path.join(SRC, 'agent-swap.mjs'), path.join(source, 'src', 'agent-swap.mjs'));
    tree('agent', '3.1.0', 'live');
    const promote = (stage) => spawnSync(process.execPath, [path.join(source, 'install', 'promote.mjs'), base, stage], { encoding: 'utf8' });

    writeFloor(base, '3.1.0');
    const older = tree('agent.install.new.1', '3.0.0', 'staged');
    const refused = promote(older);
    assert.notEqual(refused.status, 0);
    assert.match(refused.stderr, /staged agent 3\.0\.0 is below the version floor 3\.1\.0/);
    // An installer has no --allow-downgrade, so the refusal names the ways back.
    assert.ok(refused.stderr.includes('run legionctl self-update --from <bundle> --allow-downgrade from an unrestricted session'), refused.stderr);
    assert.ok(refused.stderr.includes(`lower the version in ${floorFile(base)} by hand`), refused.stderr);
    assert.equal(versionOf(path.join(base, 'agent')), '3.1.0');
    assert.equal(fs.existsSync(older), true);

    writeFloor(base, '3.0.0');
    const promoted = promote(older);
    assert.equal(promoted.status, 0, promoted.stderr);
    assert.equal(versionOf(path.join(base, 'agent')), '3.0.0');
    assert.deepEqual(readFloor(base), { schema: 1, version: '3.0.0', updatedAt: '2026-01-01T00:00:00.000Z', reason: 'install' }, 'a release that declares no floor raises none');

    // A committed promotion raises the floor to what the release declares, as
    // the agent's own install does, so the launcher holds the next one to it.
    const raised = promote(tree('agent.install.new.2', '3.2.0', 'newer', { floor: '3.2.0' }));
    assert.equal(raised.status, 0, raised.stderr);
    assert.deepEqual([readFloor(base).version, readFloor(base).reason], ['3.2.0', 'install']);
    if (process.platform !== 'win32') assert.equal(fs.statSync(floorFile(base)).mode & 0o777, 0o600, 'only this user reads or writes the floor');

    // The agent refuses a bundle that declares a floor above its own version,
    // and the installer refuses the same tree.
    const above = promote(tree('agent.install.new.3', '3.3.0', 'above', { floor: '3.4.0' }));
    assert.notEqual(above.status, 0);
    assert.match(above.stderr, /declares agentFloor "3\.4\.0", which is not an x\.y\.z version at most 3\.3\.0/);
    assert.equal(versionOf(path.join(base, 'agent')), '3.2.0');

    // A floor file that cannot be read is not written over.
    fs.writeFileSync(floorFile(base), 'not json');
    const unreadable = promote(tree('agent.install.new.4', '3.3.0', 'unreadable', { floor: '3.3.0' }));
    assert.equal(unreadable.status, 0, unreadable.stderr);
    assert.equal(versionOf(path.join(base, 'agent')), '3.3.0');
    assert.equal(fs.readFileSync(floorFile(base), 'utf8'), 'not json');
    assert.match(unreadable.stderr, /version floor was not raised/);

    // An unsigned tree vouches for nothing, so its declaration raises nothing.
    writeFloor(base, '3.3.0');
    const unsigned = promote(tree('agent.install.new.5', '3.4.0', 'checkout', { signed: false, floor: '3.4.0' }));
    assert.equal(unsigned.status, 0, unsigned.stderr);
    assert.equal(readFloor(base).version, '3.3.0');
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

// ---------------------------------------------------------------------------
// Status and doctor
// ---------------------------------------------------------------------------

test('status and doctor report the floor in effect, and doctor says when the agent is below it', () => {
  // A copy of the agent whose package.json declares its own floor, so what is
  // checked is the reading of a declaration rather than this release's.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-floor-status-'));
  try {
    fs.cpSync(SRC, path.join(root, 'src'), { recursive: true });
    const metadata = JSON.parse(fs.readFileSync(path.join(AGENT, 'package.json'), 'utf8'));
    delete metadata.legionControl;
    fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ ...metadata, version: '3.4.0', legionControl: { agentFloor: '3.2.0' } }));
    const home = path.join(root, 'home');
    const run = (command) => JSON.parse(spawnSync(process.execPath, [path.join(root, 'src', 'index.mjs'), command], {
      encoding: 'utf8', env: { ...process.env, LEGIONCTL_HOME: home }, timeout: 60_000,
    }).stdout);
    const floorCheck = () => run('doctor').checks.find((check) => check.id === 'agent.floor');

    assert.equal(run('status').agent.floor, '3.2.0', 'with no file, the floor is what the running tree declares');
    assert.equal(floorCheck().level, 'ok');

    fs.mkdirSync(home, { recursive: true });
    writeFloor(home, '3.3.0');
    assert.equal(run('status').agent.floor, '3.3.0', 'the recorded floor counts when it is higher');
    writeFloor(home, '3.0.0');
    assert.equal(run('status').agent.floor, '3.2.0', 'and the declaration when the record is lower');

    writeFloor(home, '99.0.0');
    const below = floorCheck();
    assert.equal(below.level, 'fail');
    assert.match(below.summary, /agent 3\.4\.0 is below the version floor 99\.0\.0/);
    assert.ok(below.fix);

    fs.writeFileSync(floorFile(home), '[]');
    assert.equal(run('status').agent.floor, null, 'a floor that cannot be read is not stated');
    const unreadable = floorCheck();
    assert.equal(unreadable.level, 'warn');
    assert.ok(unreadable.fix.includes(floorFile(home)));

    fs.rmSync(floorFile(home));
    fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ ...metadata, version: '3.4.0' }));
    assert.equal(run('status').agent.floor, null, 'nothing recorded and nothing declared');
    assert.equal(floorCheck().level, 'warn');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
