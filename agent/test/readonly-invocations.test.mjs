import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { cli, fixtureConfig, fixtureData, withHome, writeConfig } from './helpers.mjs';

function inventory(home) {
  return fs.readdirSync(home, { recursive: true }).sort().map((name) => {
    const file = path.join(home, name);
    const stat = fs.statSync(file);
    return [name, stat.mtimeMs, stat.isFile() ? fs.readFileSync(file).toString('hex') : null];
  });
}

test('policy, cycle preview, admin preview and bundle checks leave config and legacy state untouched', async () => withHome(async (home) => {
  writeConfig(home, fixtureConfig(fixtureData(home)));
  fs.writeFileSync(path.join(home, 'state.json'), JSON.stringify({ services: { demo: { pauseUntil: '2099-01-01T00:00:00.000Z' } } }));
  const before = inventory(home);
  for (const args of [['policy'], ['policy', '--service', 'demo'], ['cycle', '--dry-run']]) {
    const result = cli(args, { home });
    assert.equal(result.payload.ok, true, result.stdout);
    assert.deepEqual(inventory(home), before, args.join(' '));
  }
  const current = cli(['service-config', 'get'], { home }).payload;
  assert.equal(current.ok, true);
  const preview = cli(['service-config', 'validate', '--stdin'], {
    home, stdin: JSON.stringify({ expectedHash: current.hash, document: current.document }),
  });
  assert.equal(preview.payload.valid, true, preview.stdout);
  assert.deepEqual(inventory(home), before);
  const check = cli(['self-update', '--check', '--stdin'], { home, stdin: 'invalid bundle' });
  assert.equal(check.payload.ok, false);
  assert.notEqual(check.payload.reasonCode, 'config-invalid');
  assert.deepEqual(inventory(home), before);
}));

// Every invocation below gets past its arguments, so it loads config.json, and
// is then answered or refused without touching the machine. Which of them keep
// the last good copy is the command table's to say, one invocation at a time.
test('only invocations that may change something keep the last good copy of config.json', async () => withHome(async (home) => {
  writeConfig(home, fixtureConfig(fixtureData(home)));
  const lastGood = path.join(home, 'config.last-good.json');
  const keeps = (args, stdin = null) => {
    fs.rmSync(lastGood, { force: true });
    const result = cli(args, { home, stdin });
    assert.notEqual(result.payload, null, `${args.join(' ')}: ${result.stdout}${result.stderr}`);
    return fs.existsSync(lastGood);
  };
  const reads = [
    [['status', '--budget-ms', '5000']], [['busy']], [['op', 'missing-op-0001']], [['history']], [['logs']], [['doctor']],
    [['version']], [['config']], [['config', 'meta']], [['policy']], [['policy', '--service', 'demo']], [['cycle', '--dry-run']],
    [['self-update', '--check', '--stdin'], 'invalid bundle'],
  ];
  for (const [args, stdin] of reads) assert.equal(keeps(args, stdin), false, `${args.join(' ')} promises to change nothing`);
  const changes = [
    [['policy', 'set'], '{}'], [['auto-update']], [['update', '--service', 'missing']], [['restart', '--service', 'missing']],
    [['run', 'missing']], [['boot', 'missing', '--no-reboot']], [['cancel', 'missing-op-0001']], [['config', 'set'], ''],
    [['self-update', '--stdin'], 'invalid bundle'], [['op-run', 'missing-op-0001']], [['cycle']],
  ];
  for (const [args, stdin] of changes) assert.equal(keeps(args, stdin), true, `${args.join(' ')} may change something`);
}));

test('of the invocations that may change something, only those that never read config.json answer when it is broken', async () => withHome(async (home) => {
  fs.writeFileSync(path.join(home, 'config.json'), '{broken');
  const refused = (args, stdin = null) => cli(args, { home, stdin }).payload?.reasonCode === 'config-invalid';
  for (const [args, stdin] of [[['config']], [['config', 'meta']], [['self-update', '--check', '--stdin'], 'invalid bundle']]) {
    assert.equal(refused(args, stdin), false, args.join(' '));
  }
  for (const [args, stdin] of [
    [['policy']], [['policy', 'set'], '{}'], [['cycle', '--dry-run']], [['cycle']], [['update', '--service', 'demo']],
    [['config', 'set'], ''], [['self-update', '--stdin'], 'invalid bundle'], [['cancel', 'missing-op-0001']],
  ]) {
    assert.equal(refused(args, stdin), true, args.join(' '));
  }
}));

test('document reads and bundle validation remain available with malformed machine config; policy writes refuse it', async () => withHome(async (home) => {
  fs.writeFileSync(path.join(home, 'config.json'), '{broken');
  const before = inventory(home);
  for (const args of [['config'], ['config', 'meta']]) {
    const result = cli(args, { home });
    assert.equal(result.payload.ok, true, result.stdout);
  }
  const check = cli(['self-update', '--check', '--stdin'], { home, stdin: 'invalid bundle' });
  assert.notEqual(check.payload.reasonCode, 'config-invalid');
  const write = cli(['policy', 'set'], { home, stdin: '{"automatic":false}' });
  assert.equal(write.payload.reasonCode, 'config-invalid');
  assert.deepEqual(inventory(home), before);
}));
