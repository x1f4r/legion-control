import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { buildBundle, redactConfig } from '../src/doctor.mjs';
import { log } from '../src/log.mjs';
import { cli, withHome, writeConfig } from './helpers.mjs';

test('bundle redacts short arbitrary argv and configured credential echoes across every evidence section', async () => withHome(async () => {
  const canaries = ['q7x', 'r8z', 'p4k', 'v2m', 'n6w', 'ab%2B12', 'ab+12'];
  const config = {
    actions: [{ command: ['/usr/bin/tool', '--plain', canaries[0], `--setting=${canaries[1]}`] }],
    identityFile: canaries[2], nested: { apiKey: canaries[3] }, url: `https://user:${canaries[4]}@example.invalid/?api_key=${canaries[5]}`,
  };
  const echoed = canaries.join(' | ');
  const input = {
    config, doctor: { checks: [{ summary: echoed, detail: echoed }] },
    status: { services: [{ process: { detail: echoed } }] },
    history: { operations: [{ result: { output: echoed }, log: [{ line: echoed }] }] },
    logs: { lines: [{ line: echoed }] },
  };
  const bundle = await buildBundle(input);
  for (const value of canaries) assert.equal(JSON.stringify(bundle).includes(value), false, value);
  assert.equal(bundle.config.actions[0].command[0], '/usr/bin/tool');
  assert.deepEqual(bundle.config.actions[0].command.slice(1), ['<redacted>', '<redacted>', '<redacted>']);
  assert.equal(input.history.operations[0].result.output, echoed, 'sanitization must not mutate live evidence');
  assert.equal(redactConfig(config).identityFile, '<redacted>');
}));

test('an argument that is also a contract word is redacted from prose and kept where a client decodes it', async () => withHome(async () => {
  // Real argv is full of words the contract also uses: systemctl restart, a
  // probe called busy. The export must still decode, so identifiers, enum
  // values, versions and timestamps stay as they are.
  const config = { services: [{ id: 'demo', process: { start: ['systemctl', 'restart', 'demo'] }, busy: { command: ['probe', 'busy'] } }] };
  const summary = { id: 'aaaaaaaa-1111-4111-8111-111111111111', kind: 'restart', service: 'demo', state: 'finished', phase: 'done', action: 'deferred', reasonCode: 'busy' };
  const bundle = await buildBundle({
    config,
    doctor: { checks: [{ id: 'service.demo.busy', level: 'warn', summary: 'probe busy said restart later', detail: null, fix: 'run restart by hand' }] },
    status: { services: [{ id: 'demo', updates: { deferredReason: 'busy' }, lastOperation: summary }] },
    history: { operations: [summary] },
    logs: { lines: [{ at: null, command: 'restart', line: 'systemctl restart demo' }] },
  });
  assert.deepEqual(bundle.history.operations[0], summary);
  assert.equal(bundle.status.services[0].updates.deferredReason, 'busy');
  assert.equal(bundle.doctor.checks[0].id, 'service.demo.busy');
  assert.equal(bundle.logs.lines[0].command, 'restart');
  assert.equal(bundle.doctor.checks[0].summary, 'probe <redacted> said <redacted> later');
  assert.equal(bundle.logs.lines[0].line, 'systemctl <redacted> <redacted>');
}));

test('JSON-escaped argument echoes and raw-config values omitted by normalization are removed from exports', async () => withHome(async () => {
  const canary = 'q7x"r8z\\p4k';
  const rawOnly = 's9t';
  const bundle = await buildBundle({ config: { actions: [{ command: ['tool', canary] }] }, rawConfig: { privateKey: rawOnly },
    doctor: { detail: rawOnly }, status: {}, history: {}, logs: { line: JSON.stringify(canary) } });
  const text = JSON.stringify(bundle);
  assert.equal(text.includes('q7x'), false);
  assert.equal(text.includes(rawOnly), false);
  assert.ok(bundle.redacted.some((line) => line.includes('unrecognized secrets')));
}));

test('CLI export omits an echoed short argument while normal action and log interfaces preserve it', async () => withHome(async (home) => {
  const canary = 'q7x';
  const echo = path.join(home, 'echo.cjs');
  fs.writeFileSync(echo, 'process.stdout.write(process.argv[2])');
  writeConfig(home, {
    configVersion: 3, services: [], system: { id: 'test', name: 'Test' }, updates: { automatic: false },
    // The action has an id of its own: an identifier is part of the reply's
    // shape, never redacted, and only the argument is private.
    actions: [{ id: 'echo', name: 'Echo', command: [process.execPath, echo, canary] }],
    sleep: { command: [process.execPath, echo, canary] },
  });
  const ran = cli(['run', 'echo'], { home });
  assert.equal(ran.payload.ok, true, ran.stdout);
  assert.equal(ran.payload.output, canary);
  assert.ok(JSON.stringify(cli(['op', ran.payload.op.id], { home }).payload).includes(canary));
  assert.equal(cli(['history'], { home }).payload.operations[0].actionId, 'echo');
  log(`configured output ${canary}`, 'run');
  assert.ok(JSON.stringify(cli(['logs'], { home }).payload).includes(canary));
  const doctor = cli(['doctor'], { home }).payload;
  assert.equal(JSON.stringify(doctor).includes(canary), false);
  assert.ok(doctor.checks.every((entry) => ['ok', 'warn', 'fail'].includes(entry.level)));
  const bundle = cli(['bundle'], { home }).payload;
  assert.equal(bundle.ok, true);
  assert.equal(JSON.stringify(bundle).includes(canary), false);
  assert.equal(bundle.history.operations[0].actionId, 'echo', 'identifiers survive so the export still decodes');
  assert.ok(JSON.stringify(bundle.config).includes('<redacted>'));
  assert.ok(JSON.stringify(cli(['logs'], { home }).payload).includes(canary), 'export must not rewrite retained logs');
}));
