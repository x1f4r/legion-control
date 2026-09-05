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
